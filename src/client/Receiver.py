import asyncio
import json
import signal
import sys
import time
from base64 import b64decode
from Crypto.Cipher import AES, PKCS1_OAEP
from Crypto.PublicKey import RSA
import ssl

# Load configuration
try:
    with open("config.json", "r") as config_file:
        config = json.load(config_file)
    QUEUE_NAME = config["queue_name"]
    EXCHANGE_NAME = config["exchange_name"]
    ROUTING_KEY = config["routing_key"]
    SERVER_ADDRESS = config["server_address"]
    SERVER_PORT = config["server_port"]
    TLS_CONFIG = config["tls"]
except FileNotFoundError:
    print("❌ [RECEIVER] Configuration file 'config.json' not found.")
    sys.exit(1)
except KeyError as e:
    print(f"❌ [RECEIVER] Missing key in configuration file: {e}")
    sys.exit(1)

# Thread-safe queues
message_queue = asyncio.Queue()
temp_message_queue = asyncio.Queue()  # Temporary queue for messages received during ACK
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

# Configure SSL context from config
ssl_context = ssl.SSLContext(getattr(ssl, TLS_CONFIG["protocol"]))
ssl_context.load_verify_locations(TLS_CONFIG["certificate_path"])  # Load server certificate
ssl_context.verify_mode = getattr(ssl, TLS_CONFIG["verify_mode"])
ssl_context.check_hostname = TLS_CONFIG["check_hostname"]

# Performs hybrid decryption
async def decrypt_message_hybrid(data: dict) -> str:
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

# Sends acknowledgment with retries
async def send_ack_with_retry(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, message_id: str):
    max_retries = 3
    timeout = 5
    for attempt in range(max_retries):
        try:
            print(f"📤 [RECEIVER] Sending ACK for message {message_id} (Attempt {attempt + 1}/{max_retries})")
            writer.write(f"ack {message_id}\n".encode('utf-8'))
            await writer.drain()

            start_time = time.time()
            while time.time() - start_time < timeout:
                try:
                    response = (await asyncio.wait_for(reader.readline(), timeout=1.0)).decode('utf-8').strip()
                    print(f"🔍 [RECEIVER] ACK response: {response}")
                    if response.startswith(f"ACK_CONFIRMED {message_id}"):
                        print(f"✅ [RECEIVER] Server confirmed ACK for message {message_id}: {response}")
                        return True
                    elif response.startswith("Message:"):
                        print(f"🔄 [RECEIVER] Received new message in ACK response, queuing: {response}")
                        await temp_message_queue.put(response)
                    elif response.startswith("ACK_CONFIRMED"):
                        print(f"⚠️ [RECEIVER] Received ACK for another message: {response}")
                    else:
                        print(f"⚠️ [RECEIVER] Unexpected server response: {response}")
                except asyncio.TimeoutError:
                    continue
            print(f"🔁 [RECEIVER] No correct ACK received, retrying ({attempt + 1}/{max_retries})...")
        except Exception as e:
            print(f"❌ [RECEIVER] Error sending ACK for message {message_id}: {e}")
        await asyncio.sleep(2 ** attempt)
    print(f"❌ [RECEIVER] Failed to acknowledge message {message_id}")
    return False

# Processes a single message
async def process_message(message: str, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    if message.startswith("Message:"):
        try:
            parts = message.split(" ", 2)
            if len(parts) < 3:
                print("❌ [RECEIVER] Invalid message format")
                return
            message_id = parts[1]
            if message_id in processed_messages:
                print(f"⚠️ [RECEIVER] Duplicate message {message_id}, sending ACK")
                await send_ack_with_retry(reader, writer, message_id)
                return
            message_data = json.loads(parts[2])
            print(f"🔍 [RECEIVER] Processing message {message_id}")
            decrypted = await decrypt_message_hybrid(message_data)
            if decrypted:
                print(f"✅ [RECEIVER] Decrypted message {message_id}: {decrypted}")
                await message_queue.put((message_id, decrypted))
                processed_messages.add(message_id)
                await send_ack_with_retry(reader, writer, message_id)
            else:
                print(f"❌ [RECEIVER] Decryption failed for message {message_id}")
        except json.JSONDecodeError as e:
            print(f"❌ [RECEIVER] JSON error in message parsing: {e}")
        except Exception as e:
            print(f"❌ [RECEIVER] Error processing message: {e}")

# Receives and processes messages
async def receive_messages(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    global running
    try:
        print(f"🚀 [RECEIVER] Declaring queue {QUEUE_NAME}")
        writer.write(f"declare_queue {QUEUE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"✅ [RECEIVER] Server response for queue declaration: {response}")

        print(f"🚀 [RECEIVER] Declaring exchange {EXCHANGE_NAME}")
        writer.write(f"declare_exchange {EXCHANGE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"✅ [RECEIVER] Server response for exchange declaration: {response}")

        print(f"🚀 [RECEIVER] Binding queue {QUEUE_NAME} to exchange {EXCHANGE_NAME}")
        writer.write(f"bind {QUEUE_NAME} {EXCHANGE_NAME} {ROUTING_KEY}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"✅ [RECEIVER] Server response for binding: {response}")

        print(f"🚀 [RECEIVER] Subscribing to queue {QUEUE_NAME}")
        writer.write(f"consume {QUEUE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"✅ [RECEIVER] Server response for subscription: {response}")

        while running:
            try:
                try:
                    message = await asyncio.wait_for(temp_message_queue.get(), timeout=0.1)
                    print(f"📩 [RECEIVER] Processing queued message: {message}")
                    await process_message(message, reader, writer)
                    temp_message_queue.task_done()
                    continue
                except asyncio.TimeoutError:
                    pass

                line = await asyncio.wait_for(reader.readline(), timeout=1.0)
                if not line:
                    print("🔌 [RECEIVER] Connection closed.")
                    break

                message = line.decode('utf-8').strip()
                if not message:
                    continue

                print(f"📩 [RECEIVER] Raw message received: {message}")
                if message.startswith("Message:"):
                    await process_message(message, reader, writer)
                elif message.startswith("ACK_CONFIRMED"):
                    print(f"✅ [RECEIVER] Received ACK confirmation: {message}")
                else:
                    print(f"⚠️ [RECEIVER] Unhandled message: {message}")
            except asyncio.TimeoutError:
                continue
            except Exception as e:
                print(f"❌ [RECEIVER] Error receiving messages: {e}")
                break
    finally:
        writer.close()
        await writer.wait_closed()
        print("🔌 [RECEIVER] Socket closed.")

# Processes decrypted messages with batching in JSONL format
async def process_messages():
    global running
    print("📝 [RECEIVER] Starting message processing...")
    batch_size = 10
    batch = []
    try:
        with open("received_messages.jsonl", "a", encoding='utf-8') as f:
            while running:
                try:
                    message_id, message = await asyncio.wait_for(message_queue.get(), timeout=1.0)
                    print(f"📥 [RECEIVER] Message read from queue: {message_id}, {message}")
                    batch.append({"message_id": message_id, "message": message, "timestamp": time.time()})
                    if len(batch) >= batch_size:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        print(f"💾 [RECEIVER] Batch of {len(batch)} messages saved to received_messages.jsonl")
                        batch.clear()
                    message_queue.task_done()
                except asyncio.TimeoutError:
                    if batch:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        print(f"💾 [RECEIVER] Batch of {len(batch)} messages saved to received_messages.jsonl")
                        batch.clear()
                except Exception as e:
                    print(f"❌ [RECEIVER] Error saving message: {e}")
    except Exception as e:
        print(f"❌ [RECEIVER] Error opening/writing file: {e}")
    finally:
        print("🛑 [RECEIVER] Message processing stopped.")

# Handles SIGINT for graceful shutdown
def signal_handler(loop):
    global running
    print("🛑 [RECEIVER] Received SIGINT, shutting down...")
    running = False
    loop.stop()
    loop.run_until_complete(loop.shutdown_asyncgens())
    loop.close()
    sys.exit(0)

# Main function with reconnection logic
async def main():
    loop = asyncio.get_running_loop()
    signal.signal(signal.SIGINT, lambda s, f: signal_handler(loop))
    print("🚀 [RECEIVER] Starting message processing task...")
    asyncio.create_task(process_messages())

    while running:
        try:
            reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
            print(f"✅ [RECEIVER] TLS connection established. Cipher: {writer.get_extra_info('cipher')}")
            await receive_messages(reader, writer)
        except Exception as e:
            print(f"🔁 [RECEIVER] Connection failed: {e}. Retrying in 1 second...")
            await asyncio.sleep(1)

if __name__ == "__main__":
    asyncio.run(main())