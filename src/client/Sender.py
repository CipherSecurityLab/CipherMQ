import json
import asyncio
import uuid
import time
from base64 import b64encode
from Crypto.Cipher import AES, PKCS1_OAEP
from Crypto.PublicKey import RSA
from Crypto.Random import get_random_bytes
import ssl

# Load configuration
try:
    with open("config.json", "r") as config_file:
        config = json.load(config_file)
    EXCHANGE_NAME = config["exchange_name"]
    ROUTING_KEY = config["routing_key"]
    SERVER_ADDRESS = config["server_address"]
    SERVER_PORT = config["server_port"]
    TLS_CONFIG = config["tls"]
except FileNotFoundError:
    print("❌ [SENDER] Configuration file 'config.json' not found.")
    sys.exit(1)
except KeyError as e:
    print(f"❌ [SENDER] Missing key in configuration file: {e}")
    sys.exit(1)

# Stores pending messages until acknowledged
pending_messages = {}
message_queue = asyncio.Queue()

# Configure SSL context from config
ssl_context = ssl.SSLContext(getattr(ssl, TLS_CONFIG["protocol"]))
ssl_context.load_verify_locations(TLS_CONFIG["certificate_path"])  # Load server certificate
ssl_context.verify_mode = getattr(ssl, TLS_CONFIG["verify_mode"])
ssl_context.check_hostname = TLS_CONFIG["check_hostname"]

# Performs hybrid encryption: RSA encrypts a random session key, and AES-GCM encrypts the message
async def encrypt_message_hybrid(message: str, public_key_path: str) -> dict:
    try:
        # Load public key
        with open(public_key_path, 'rb') as f:
            recipient_key = RSA.import_key(f.read())
        cipher_rsa = PKCS1_OAEP.new(recipient_key)

        # Generate session key and encrypt with RSA
        session_key = get_random_bytes(16)
        enc_session_key = cipher_rsa.encrypt(session_key)

        # Encrypt message with AES-GCM
        cipher_aes = AES.new(session_key, AES.MODE_GCM)
        ciphertext, tag = cipher_aes.encrypt_and_digest(message.encode('utf-8'))

        # Generate final output
        message_id = str(uuid.uuid4())
        encrypted_data = {
            'message_id': message_id,
            'enc_session_key': b64encode(enc_session_key).decode('utf-8'),
            'nonce': b64encode(cipher_aes.nonce).decode('utf-8'),
            'tag': b64encode(tag).decode('utf-8'),
            'ciphertext': b64encode(ciphertext).decode('utf-8'),
        }

        pending_messages[message_id] = encrypted_data
        print(f"📤 [SENDER] Message {message_id} added to pending messages")
        return encrypted_data
    except Exception as e:
        print(f"❌ [SENDER] Error in hybrid encryption: {e}")
        return None

# Sends a batch of messages with retries
async def send_message_batch(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, messages: list, public_key_path: str):
    batch = []
    message_ids = []
    
    # Encrypt each message
    for message in messages:
        encrypted_data = await encrypt_message_hybrid(message, public_key_path)
        if encrypted_data:
            batch.append(encrypted_data)
            message_ids.append(encrypted_data['message_id'])
    
    if not batch:
        print("❌ [SENDER] No messages encrypted successfully.")
        return

    max_retries = 3
    timeout = 5  # seconds
    command = f"publish_batch {EXCHANGE_NAME} {ROUTING_KEY} {json.dumps(batch, ensure_ascii=False)}\n"
    print(f"🔍 [SENDER] Sending command: {command}")

    for attempt in range(max_retries):
        try:
            print(f"🚀 [SENDER] Sending batch of {len(batch)} messages (Attempt {attempt + 1}/{max_retries})")
            writer.write(command.encode('utf-8'))
            await writer.drain()

            # Wait for acknowledgment for each message
            response = (await asyncio.wait_for(reader.read(4096), timeout=timeout)).decode('utf-8').strip()
            print(f"🔍 [SENDER] Server response: {response}")
            for message_id in message_ids:
                if f"ACK {message_id}" in response:
                    print(f"✅ [SENDER] Server ACK received for message {message_id}")
                    pending_messages.pop(message_id, None)
                else:
                    print(f"⚠️ [SENDER] Unexpected server response for message {message_id}: {response}")
            if all(f"ACK {mid}" in response for mid in message_ids):
                return
        except asyncio.TimeoutError:
            print(f"🔁 [SENDER] Timeout for batch, retrying ({attempt + 1}/{max_retries})...")
        except Exception as e:
            print(f"❌ [SENDER] Error sending batch: {e}")
        await asyncio.sleep(2 ** attempt)  # Exponential backoff
    print(f"❌ [SENDER] Failed to send batch after {max_retries} attempts.")

# Collects messages into a batch and sends them
async def collect_and_send_messages(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, public_key_path: str, batch_size: int = 5, batch_timeout: float = 1.0):
    max_batch_size = 10  # Limit batch size to avoid large payloads
    batch = []
    start_time = time.time()

    # Collect messages from queue until batch_size or timeout
    while len(batch) < batch_size and (time.time() - start_time) < batch_timeout:
        try:
            message = await asyncio.wait_for(message_queue.get(), timeout=batch_timeout)
            batch.append(message)
            message_queue.task_done()
        except asyncio.TimeoutError:
            break
        await asyncio.sleep(0.0001)  # Small sleep to allow other coroutines

    print(f"📦 [SENDER] Collected batch of {len(batch)} messages")
    # Split batch if it exceeds max_batch_size
    for i in range(0, len(batch), max_batch_size):
        chunk = batch[i:i + max_batch_size]
        if chunk:
            await send_message_batch(reader, writer, chunk, public_key_path)

    # Send any remaining messages in the queue
    while not message_queue.empty():
        try:
            batch = []
            while len(batch) < max_batch_size and not message_queue.empty():
                message = await message_queue.get()
                batch.append(message)
                message_queue.task_done()
            if batch:
                print(f"📦 [SENDER] Sending remaining batch of {len(batch)} messages")
                await send_message_batch(reader, writer, batch, public_key_path)
        except Exception as e:
            print(f"❌ [SENDER] Error processing remaining messages: {e}")

# Producer task to add messages to the queue
async def produce_messages(messages: list):
    for message in messages:
        await message_queue.put(message)
        print(f"📝 [SENDER] Queued message: {message}")
        await asyncio.sleep(0.001)  # Simulate some delay between messages

async def main():
    messages = ["This is a test hybrid message."] * 5  # Example: 5 messages
    print(f"📝 [SENDER] Preparing to send {len(messages)} messages")
    try:
        reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
        print(f"✅ [SENDER] TLS connection established. Cipher: {writer.get_extra_info('cipher')}")
        try:
            # Start producer task
            producer_task = asyncio.create_task(produce_messages(messages))
            # Wait for producer to finish
            await producer_task
            # Collect and send messages in batches
            await collect_and_send_messages(reader, writer, 'receiver_public.pem')
        finally:
            writer.close()
            await writer.wait_closed()
    except Exception as e:
        print(f"❌ [SENDER] Connection failed: {e}")

if __name__ == '__main__':
    asyncio.run(main())