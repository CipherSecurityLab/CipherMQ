import asyncio
import json
import signal
import ssl
import sys
import os
import time
from datetime import datetime
from base64 import b64decode, b64encode
from cryptography import x509
from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives import serialization
from nacl.public import PrivateKey, SealedBox

# Extract client_id from client certificate
def extract_client_id(tls_config):
    try:
        with open(tls_config["client_cert_path"], "rb") as cert_file:
            cert_data = cert_file.read()
        cert = x509.load_pem_x509_certificate(cert_data, default_backend())
        cn = cert.subject.get_attributes_for_oid(x509.oid.NameOID.COMMON_NAME)
        if not cn:
            raise ValueError("No Common Name found in client certificate")
        return cn[0].value
    except Exception as e:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [RECEIVER] Error extracting client_id from certificate: {e}")
        sys.exit(1)

# Load configuration
try:
    os.makedirs("certs", exist_ok=True)
    with open("config.json", "r") as config_file:
        config = json.load(config_file)
    QUEUE_NAME = config["queue_name"]
    EXCHANGE_NAME = config["exchange_name"]
    ROUTING_KEY = config["routing_key"]
    SERVER_ADDRESS = config["server_address"]
    SERVER_PORT = config["server_port"]
    TLS_CONFIG = config["tls"]
    CLIENT_ID = extract_client_id(TLS_CONFIG)
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Extracted client_id from certificate: {CLIENT_ID}")
except FileNotFoundError:
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [RECEIVER] Configuration file 'config - receiver.json' not found.")
    sys.exit(1)
except KeyError as e:
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [RECEIVER] Missing key in configuration file: {e}")
    sys.exit(1)

# Thread-safe queues
message_queue = asyncio.Queue()
temp_message_queue = asyncio.Queue()
running = True
processed_messages = set()

# Load private and public keys
try:
    with open("certs/receiver_private.key", "r") as key_file:
        private_key_bytes = b64decode(key_file.read())
        PRIVATE_KEY = PrivateKey(private_key_bytes)
    with open("certs/receiver_public.key", "r") as key_file:
        PUBLIC_KEY = PrivateKey(private_key_bytes).public_key
except Exception as e:
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [RECEIVER] Error loading keys: {e}")
    sys.exit(1)

# Configure SSL context for mTLS
ssl_context = ssl.SSLContext(getattr(ssl, TLS_CONFIG["protocol"]))
ssl_context.load_verify_locations(TLS_CONFIG["certificate_path"])
ssl_context.load_cert_chain(
    certfile=TLS_CONFIG["client_cert_path"],
    keyfile=TLS_CONFIG["client_key_path"]
)
ssl_context.verify_mode = getattr(ssl, TLS_CONFIG["verify_mode"])
ssl_context.check_hostname = TLS_CONFIG["check_hostname"]

# Register public key with server
async def register_public_key(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    public_key_b64 = b64encode(PUBLIC_KEY._public_key).decode('utf-8')
    command = f"register_key {CLIENT_ID} {public_key_b64}\n"
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Sending command: {command.strip()}")
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Server response for public key registration: {response}")
    if response != "Public key registered":
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Failed to register public key")
        return False
    return True

# Decrypt message using private key with hybrid encryption
async def decrypt_message_hybrid(data: dict) -> str:
    try:
        required_keys = {"enc_session_key", "nonce", "tag", "ciphertext", "message_id"}
        if not required_keys.issubset(data.keys()):
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Incomplete message for hybrid decryption: {data}")
            return None
        enc_session_key = b64decode(data['enc_session_key'])
        nonce = b64decode(data['nonce'])
        tag = b64decode(data['tag'])
        ciphertext = b64decode(data['ciphertext'])
        sealed_box = SealedBox(PRIVATE_KEY)
        session_key = sealed_box.decrypt(enc_session_key)
        cipher = AESGCM(session_key)
        encrypted_bytes = ciphertext + tag
        plaintext = cipher.decrypt(nonce, encrypted_bytes, None)
        return plaintext.decode('utf-8')
    except Exception as e:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error in hybrid decryption: {e}")
        return None

# Sends acknowledgment with retries
async def send_ack_with_retry(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, message_id: str):
    max_retries = 3
    timeout = 10
    for attempt in range(max_retries):
        try:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Sending ACK for message {message_id} (Attempt {attempt + 1}/{max_retries})")
            writer.write(f"ack {message_id}\n".encode('utf-8'))
            await writer.drain()
            start_time = time.time()
            while time.time() - start_time < timeout:
                try:
                    response = (await asyncio.wait_for(reader.readline(), timeout=2.0)).decode('utf-8').strip()
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] ACK response: {response}")
                    if response.startswith(f"ACK_CONFIRMED {message_id}"):
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Server confirmed ACK for message {message_id}")
                        return True
                    elif response.startswith("Message:"):
                        parts = response.split(" ", 2)
                        if len(parts) >= 2 and parts[1] not in processed_messages:
                            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Received new message in ACK response, queuing: {response}")
                            await temp_message_queue.put(response)
                    elif response.startswith("ACK_CONFIRMED"):
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Received ACK for another message: {response}")
                    else:
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Unexpected server response: {response}")
                except asyncio.TimeoutError:
                    continue
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] No correct ACK received, retrying ({attempt + 1}/{max_retries})")
        except Exception as e:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error sending ACK for message {message_id}: {e}")
        await asyncio.sleep(2 ** attempt)
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Failed to acknowledge message {message_id}")
    return False

# Processes a single message
async def process_message(message: str, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    if message.startswith("Message:"):
        try:
            parts = message.split(" ", 2)
            if len(parts) < 3:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Invalid message format")
                return
            message_id = parts[1]
            if message_id in processed_messages:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Duplicate message {message_id}, sending ACK")
                await send_ack_with_retry(reader, writer, message_id)
                return
            message_data = json.loads(parts[2])
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Processing message {message_id}")
            decrypted = await decrypt_message_hybrid(message_data)
            if decrypted:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Decrypted message {message_id}: {decrypted}")
                await message_queue.put((message_id, decrypted))
                processed_messages.add(message_id)
                await send_ack_with_retry(reader, writer, message_id)
            else:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Decryption failed for message {message_id}")
        except json.JSONDecodeError as e:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] JSON error in message parsing: {e}")
        except Exception as e:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error processing message: {e}")

# Receives and processes messages
async def receive_messages(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    global running
    try:
        if not await register_public_key(reader, writer):
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Public key registration failed. Exiting")
            return
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Declaring queue {QUEUE_NAME}")
        writer.write(f"declare_queue {QUEUE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Server response for queue declaration: {response}")
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Declaring exchange {EXCHANGE_NAME}")
        writer.write(f"declare_exchange {EXCHANGE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Server response for exchange declaration: {response}")
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Binding queue {QUEUE_NAME} to exchange {EXCHANGE_NAME}")
        writer.write(f"bind {QUEUE_NAME} {EXCHANGE_NAME} {ROUTING_KEY}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Server response for binding: {response}")
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Subscribing to queue {QUEUE_NAME}")
        writer.write(f"consume {QUEUE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Server response for subscription: {response}")
        while running:
            try:
                # Process messages from temp_message_queue first
                try:
                    message = await asyncio.wait_for(temp_message_queue.get(), timeout=0.1)
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Processing queued message: {message}")
                    if message.startswith("Message:"):
                        parts = message.split(" ", 2)
                        if len(parts) >= 2 and parts[1] in processed_messages:
                            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Skipping duplicate queued message {parts[1]}")
                            temp_message_queue.task_done()
                            continue
                        await process_message(message, reader, writer)
                    temp_message_queue.task_done()
                    continue
                except asyncio.TimeoutError:
                    pass
                # Read new messages from server
                line = await asyncio.wait_for(reader.readline(), timeout=1.0)
                if not line:
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Connection closed")
                    break
                message = line.decode('utf-8').strip()
                if not message:
                    continue
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Raw message received: {message}")
                if message.startswith("Message:"):
                    parts = message.split(" ", 2)
                    if len(parts) >= 2 and parts[1] in processed_messages:
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Duplicate message {parts[1]} from server, sending ACK")
                        await send_ack_with_retry(reader, writer, parts[1])
                        continue
                    await process_message(message, reader, writer)
                elif message.startswith("ACK_CONFIRMED"):
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Received ACK confirmation: {message}")
                else:
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Unhandled message: {message}")
            except asyncio.TimeoutError:
                continue
            except Exception as e:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error receiving messages: {e}")
                break
    finally:
        writer.close()
        await writer.wait_closed()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Connection closed")

# Processes decrypted messages with batching in JSONL format
async def process_messages():
    global running
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Starting message processing")
    batch_size = 10
    batch = []
    output_file = "data/received_messages.jsonl"
    try:
        os.makedirs(os.path.dirname(output_file) or ".", exist_ok=True)
        with open(output_file, "a", encoding='utf-8') as f:
            while running:
                try:
                    message_id, message = await asyncio.wait_for(message_queue.get(), timeout=1.0)
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Message read from queue: {message_id}")
                    batch.append({"message_id": message_id, "message": message, "timestamp": time.time()})
                    if len(batch) >= batch_size:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Batch of {len(batch)} messages saved to {output_file}")
                        batch.clear()
                    message_queue.task_done()
                except asyncio.TimeoutError:
                    if batch:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Batch of {len(batch)} messages saved to {output_file}")
                        batch.clear()
                except Exception as e:
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error saving message to {output_file}: {e}")
    except Exception as e:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error opening/writing file {output_file}: {e}")
    finally:
        if batch:
            try:
                with open(output_file, "a", encoding='utf-8') as f:
                    for item in batch:
                        json.dump(item, f, ensure_ascii=False)
                        f.write("\n")
                    f.flush()
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Final batch of {len(batch)} messages saved to {output_file}")
            except Exception as e:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Error saving final batch to {output_file}: {e}")
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Message processing stopped")

# Handles SIGINT for graceful shutdown
def signal_handler(loop):
    global running
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Received SIGINT, shutting down")
    running = False
    loop.stop()
    loop.run_until_complete(loop.shutdown_asyncgens())
    loop.close()
    sys.exit(0)

# Main function with reconnection logic
async def main():
    loop = asyncio.get_running_loop()
    signal.signal(signal.SIGINT, lambda s, f: signal_handler(loop))
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Starting message processing task")
    asyncio.create_task(process_messages())
    while running:
        try:
            reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] TLS connection established. Cipher: {writer.get_extra_info('cipher')}")
            await receive_messages(reader, writer)
        except Exception as e:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [RECEIVER] Connection failed: {e}. Retrying in 1 second")
            await asyncio.sleep(1)

if __name__ == "__main__":
    asyncio.run(main())