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
processed_messages = set()

# Load private key for hybrid decryption
try:
    with open("receiver_private.pem", "rb") as key_file:
        PRIVATE_KEY = RSA.import_key(key_file.read())
        CIPHER_RSA = PKCS1_OAEP.new(PRIVATE_KEY)
except Exception as e:
    print(f"❌ [RECEIVER] Error loading private key: {e}")
    sys.exit(1)

# Performs hybrid decryption
def decrypt_message_hybrid(data: dict) -> str:
    """Decrypt message using RSA + AES-GCM."""
    try:
        required_keys = {"enc_session_key", "nonce", "tag", "ciphertext", "message_id"}
        if not required_keys.issubset(data.keys()):
            print(f"❌ [RECEIVER] Incomplete message for hybrid decryption: {data}")
            return None

        session_key = CIPHER_RSA.decrypt(b64decode(data['enc_session_key']))
        nonce = b64decode(data['nonce'])
        tag = b64decode(data['tag'])
        ciphertext = b64decode(data['ciphertext'])

        cipher_aes = AES.new(session_key, AES.MODE_GCM, nonce=nonce)
        plaintext = cipher_aes.decrypt_and_verify(ciphertext, tag)
        return plaintext.decode('utf-8')
    except Exception as e:
        print(f"❌ [RECEIVER] Error in hybrid decryption: {e}")
        return None

# Sends acknowledgment with retries until confirmed
def send_ack_with_retry(client: socket.socket, message_id: str):
    max_retries = 3
    timeout = 5  # seconds
    for attempt in range(max_retries):
        try:
            print(f"📤 [RECEIVER] Sending ACK for message {message_id} (Attempt {attempt + 1}/{max_retries})")
            client.send(f"ack {message_id}\n".encode('utf-8'))
            client.settimeout(timeout)
            response = client.recv(1024).decode('utf-8').strip()
            if response.startswith(f"ACK_CONFIRMED {message_id}"):
                print(f"✅ [RECEIVER] Server confirmed ACK for message {message_id}: {response}")
                return True
            print(f"⚠️ [RECEIVER] Unexpected server response: {response}")
        except socket.timeout:
            print(f"🔁 [RECEIVER] Timeout on ACK for message {message_id}, retrying ({attempt + 1}/{max_retries})...")
        except Exception as e:
            print(f"❌ [RECEIVER] Error sending ACK for message {message_id}: {e}")
        time.sleep(1)
    print(f"❌ [RECEIVER] Failed to acknowledge message {message_id}")
    return False

# Receives and processes messages
def receive_messages(client: socket.socket, queue_name: str = "default_queue"):
    """Receive and process messages from the TCP server."""
    global running
    try:
        # Subscribe to the queue
        print(f"🚀 [RECEIVER] Subscribing to queue {queue_name}")
        client.send(f"consume {queue_name}\n".encode('utf-8'))
        response = client.recv(1024).decode('utf-8').strip()
        print(f"✅ [RECEIVER] Server response for subscription: {response}")

        while running:
            client.settimeout(1.0)
            try:
                data = client.recv(1024).decode('utf-8').strip()
                if not data:
                    print("🔌 [RECEIVER] Connection closed.")
                    break

                print(f"📩 [RECEIVER] Raw message received: {data}")
                if data.startswith("Message:"):
                    try:
                        parts = data.split(" ", 2)
                        if len(parts) < 3:
                            print("❌ [RECEIVER] Invalid message format")
                            continue
                        message_id = parts[1]
                        if message_id in processed_messages:
                            print(f"⚠️ [RECEIVER] Duplicate message {message_id}, sending ACK")
                            send_ack_with_retry(client, message_id)
                            continue
                        message_data = json.loads(parts[2])
                        print(f"🔍 [RECEIVER] Processing message {message_id}")
                        decrypted = decrypt_message_hybrid(message_data)
                        if decrypted:
                            print(f"✅ [RECEIVER] Decrypted message {message_id}: {decrypted}")
                            message_queue.put((message_id, decrypted))
                            processed_messages.add(message_id)
                            send_ack_with_retry(client, message_id)
                        else:
                            print(f"❌ [RECEIVER] Decryption failed for message {message_id}")
                    except json.JSONDecodeError as e:
                        print(f"❌ [RECEIVER] JSON error: {e}")
                    except Exception as e:
                        print(f"❌ [RECEIVER] Error processing message: {e}")
                else:
                    print(f"❌ [RECEIVER] Unknown message: {data}")
            except socket.timeout:
                continue
            except Exception as e:
                print(f"❌ [RECEIVER] Error receiving messages: {e}")
                break
    except Exception as e:
        print(f"❌ [RECEIVER] Error in receive_messages: {e}")
    finally:
        client.close()
        print("🔌 [RECEIVER] Socket closed.")

# Processes decrypted messages
def process_messages():
    """Process decrypted messages from the queue and save to file."""
    global running
    print("📝 [RECEIVER] Starting message processing...")
    try:
        with open("received_messages.json", "a", encoding='utf-8') as f:
            while running:
                try:
                    message_id, message = message_queue.get(timeout=1)
                    print(f"📥 [RECEIVER] Message read from queue: {message_id}, {message}")
                    json.dump(
                        {"message_id": message_id, "message": message, "timestamp": time.time()},
                        f,
                        ensure_ascii=False
                    )
                    f.write("\n")
                    f.flush()
                    print(f"💾 [RECEIVER] Message {message_id} saved to file")
                    message_queue.task_done()
                except queue.Empty:
                    continue
                except Exception as e:
                    print(f"❌ [RECEIVER] Error saving message: {e}")
    except Exception as e:
        print(f"❌ [RECEIVER] Error opening/writing file: {e}")
    finally:
        print("🛑 [RECEIVER] Message processing stopped.")

# Handles SIGINT for graceful shutdown
def signal_handler(sig, frame):
    global running
    print("🛑 [RECEIVER] Received SIGINT, shutting down...")
    running = False
    time.sleep(1)
    sys.exit(0)

# Main function with reconnection logic
def main():
    signal.signal(signal.SIGINT, signal_handler)
    print("🚀 [RECEIVER] Starting message processing thread...")
    threading.Thread(target=process_messages, daemon=False).start()

    while running:
        try:
            client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            client.connect(('127.0.0.1', 5672))
            print("✅ [RECEIVER] Connection established.")
            receive_messages(client)
        except Exception as e:
            print(f"🔁 [RECEIVER] Connection failed: {e}. Retrying in 1 second...")
            if 'client' in locals():
                client.close()
            time.sleep(1)

if __name__ == "__main__":
    main()