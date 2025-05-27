import socket
import queue
import threading
import json
import signal
import sys
import time
from base64 import b64decode
from Crypto.Cipher import AES, PKCS1_OAEP
from Crypto.PublicKey import RSA

# Thread-safe queue
message_queue = queue.Queue()
running = True

# Load private key for hybrid decryption
try:
    with open("receiver_private.pem", "rb") as key_file:
        PRIVATE_KEY = RSA.import_key(key_file.read())
        CIPHER_RSA = PKCS1_OAEP.new(PRIVATE_KEY)
except Exception as e:
    print(f"❌ Error loading private key: {e}")
    sys.exit(1)

# Performs hybrid decryption: RSA decrypts the session key, then AES-GCM decrypts the message with nonce and tag verification
def decrypt_message_hybrid(data: dict) -> str:
    """Decrypt message using RSA + AES-GCM."""
    try:
        required_keys = {"enc_session_key", "nonce", "tag", "ciphertext", "message_id"}
        if not required_keys.issubset(data.keys()):
            print(f"❌ Incomplete message for hybrid decryption: {data}")
            return None

        session_key = CIPHER_RSA.decrypt(b64decode(data['enc_session_key']))
        nonce = b64decode(data['nonce'])
        tag = b64decode(data['tag'])
        ciphertext = b64decode(data['ciphertext'])

        cipher_aes = AES.new(session_key, AES.MODE_GCM, nonce=nonce)
        plaintext = cipher_aes.decrypt_and_verify(ciphertext, tag)
        return plaintext.decode('utf-8')
    except Exception as e:
        print(f"❌ Error in hybrid decryption: {e}")
        return None

# Continuously receives messages, with a 1-second timeout to check for shutdown signals
def receive_messages(client: socket.socket, queue_name: str = "default_queue"):
    """Receive and process messages from the TCP server."""
    global running
    try:
        # Subscribe to the queue
        client.send(f"consume {queue_name}\n".encode('utf-8'))
        response = client.recv(1024).decode('utf-8').strip()
        print(f"✅ Server response: {response}")

        while running:
            client.settimeout(1.0)  # Timeout to check running flag
            try:
                data = client.recv(1024).decode('utf-8').strip()
                if not data:
                    print("🔌 Connection closed.")
                    break

                print(f"📩 Raw message received: {data}")
                if data.startswith("Message:"):
                    try:
                        # Extract message_id and JSON
                        parts = data.split(" ", 2)
                        if len(parts) < 3:
                            print("❌ Invalid message format")
                            continue
                        message_id = parts[1]
                        message_data = json.loads(parts[2])

                        # Decrypt message
                        decrypted = decrypt_message_hybrid(message_data)
                        if decrypted:
                            print(f"✅ Decrypted message: {decrypted}")
                            message_queue.put((message_id, decrypted))
                            print(f"📤 Message added to queue: {message_id}")
                            # Send acknowledgment
                            client.send(f"ack {message_id}\n".encode('utf-8'))
                            response = client.recv(1024).decode('utf-8').strip()
                            print(f"✅ Acknowledgment sent: {response}")
                        else:
                            print("❌ Decryption failed")
                    except json.JSONDecodeError as e:
                        print(f"❌ JSON error: {e}")
                    except Exception as e:
                        print(f"❌ Error processing message: {e}")
                else:
                    print(f"❌ Unknown message: {data}")
            except socket.timeout:
                continue
            except Exception as e:
                print(f"❌ Error receiving messages: {e}")
                break
    except Exception as e:
        print(f"❌ Error in receive_messages: {e}")
    finally:
        client.close()
        print("🔌 Socket closed.")

# Start processing messages
def process_messages():
    """Process decrypted messages from the queue and save to file."""
    global running
    print("📝 Starting message processing...")
    try:
        with open("received_messages.json", "a", encoding='utf-8') as f:
            while running:
                try:
                    message_id, message = message_queue.get(timeout=1)
                    print(f"📥 Message read from queue: {message_id}, {message}")
                    json.dump(
                        {"message_id": message_id, "message": message, "timestamp": time.time()},
                        f,
                        ensure_ascii=False
                    )
                    f.write("\n")
                    f.flush()
                    print(f"💾 Message saved to file: {message_id}")
                    message_queue.task_done()
                except queue.Empty:
                    continue
                except Exception as e:
                    print(f"❌ Error saving message: {e}")
    except Exception as e:
        print(f"❌ Error opening/writing file: {e}")
    finally:
        print("🛑 Message processing stopped.")

# Received SIGINT, shutting down
def signal_handler(sig, frame):
    global running
    print("🛑 Received SIGINT, shutting down...")
    running = False
    time.sleep(1)  # Allow time for file and socket cleanup
    sys.exit(0)

# Attempts to connect to the TCP server, with automatic reconnection on failure
def main():
    signal.signal(signal.SIGINT, signal_handler)
    print("🚀 Starting message processing thread...")
    threading.Thread(target=process_messages, daemon=False).start()

    while running:
        try:
            client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            client.connect(('127.0.0.1', 5672))
            print("✅ Connection established.")
            receive_messages(client)
        except Exception as e:
            print(f"🔁 Connection failed: {e}. Retrying in 1 second...")
            if 'client' in locals():
                client.close()
            time.sleep(1)

if __name__ == "__main__":
    main()