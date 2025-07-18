import asyncio
import json
import ssl
import sys
import os
import time
import uuid
import random
from datetime import datetime
from base64 import b64decode, b64encode
from cryptography import x509
from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from nacl.public import PrivateKey, PublicKey, SealedBox

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
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [SENDER] Error extracting client_id from certificate: {e}")
        sys.exit(1)

# Load configuration
try:
    os.makedirs("certs", exist_ok=True)
    with open("config.json", "r") as config_file:
        config = json.load(config_file)
    EXCHANGE_NAME = config["exchange_name"]
    ROUTING_KEY = config["routing_key"]
    SERVER_ADDRESS = config["server_address"]
    SERVER_PORT = config["server_port"]
    TLS_CONFIG = config["tls"]
    RECEIVER_CLIENT_IDS = config.get("receiver_client_ids")
    if isinstance(RECEIVER_CLIENT_IDS, str):
        RECEIVER_CLIENT_IDS = [RECEIVER_CLIENT_IDS]
    CLIENT_ID = extract_client_id(TLS_CONFIG)
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Extracted client_id from certificate: {CLIENT_ID}")
except FileNotFoundError:
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [SENDER] Configuration file 'config - sender.json' not found.")
    sys.exit(1)
except KeyError as e:
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] ❌ [SENDER] Missing key in configuration file: {e}")
    sys.exit(1)

# Stores pending messages until acknowledged
pending_messages = {}

# Configure SSL context for mTLS
ssl_context = ssl.SSLContext(getattr(ssl, TLS_CONFIG["protocol"]))
ssl_context.load_verify_locations(TLS_CONFIG["certificate_path"])
ssl_context.load_cert_chain(
    certfile=TLS_CONFIG["client_cert_path"],
    keyfile=TLS_CONFIG["client_key_path"]
)
ssl_context.verify_mode = getattr(ssl, TLS_CONFIG["verify_mode"])
ssl_context.check_hostname = TLS_CONFIG["check_hostname"]

# Get public key for a receiver from server
async def get_public_key(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, receiver_client_id: str) -> str:
    command = f"get_key {receiver_client_id}\n"
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Sending command: {command.strip()}")
    try:
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await asyncio.wait_for(reader.readline(), timeout=5)).decode('utf-8').strip()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Server response: {response}")
        if response.startswith("Public key:"):
            public_key = response.split(" ", 2)[2]
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Received public key for {receiver_client_id}")
            os.makedirs("receiver_keys", exist_ok=True)
            with open(f"receiver_keys/ {receiver_client_id}_public.key", "w") as f:
                f.write(public_key)
            return public_key
        else:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Failed to get public key for {receiver_client_id}: {response}")
            return None
    except Exception as e:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Error fetching public key for {receiver_client_id}: {e}")
        return None

# Encrypt message using receiver's public key with hybrid encryption
def encrypt_message(message: str, public_key_b64: str, receiver_client_id: str) -> dict:
    try:
        public_key_bytes = b64decode(public_key_b64)
        public_key = PublicKey(public_key_bytes)
        session_key = os.urandom(32)
        sealed_box = SealedBox(public_key)
        enc_session_key = sealed_box.encrypt(session_key)
        cipher = AESGCM(session_key)
        nonce = os.urandom(12)
        message_bytes = message.encode('utf-8')
        ciphertext = cipher.encrypt(nonce, message_bytes, None)
        tag = ciphertext[-16:]
        ciphertext = ciphertext[:-16]
        return {
            "message_id": str(uuid.uuid4()),
            "receiver_client_id": receiver_client_id,
            "enc_session_key": b64encode(enc_session_key).decode('utf-8'),
            "nonce": b64encode(nonce).decode('utf-8'),
            "tag": b64encode(tag).decode('utf-8'),
            "ciphertext": b64encode(ciphertext).decode('utf-8')
        }
    except Exception as e:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Encryption failed: {e}")
        return None

# Generate a batch of messages with counter
def generate_message_batch(count: int = 20) -> list:
    message_templates = [
        "Secure communication established with CipherMQ.",
        "Data transfer completed successfully via mTLS.",
        "Hybrid encryption ensured message security.",
        "Message dispatched through secure channel.",
        "End-to-end encryption verified for this transmission."
    ]
    messages = []
    for i in range(1, count + 1):
        message_content = random.choice(message_templates)
        message = f"Count: {i} - {message_content}"
        messages.append({"message": message})
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Generated {len(messages)} messages with counter")
    return messages

# Encrypt a batch of messages for receivers
async def encrypt_message_batch(messages: list) -> list:
    encrypted_messages = []
    message_ids = set()
    for message in messages:
        target_receiver_ids = [message.get('receiver_client_id')] if message.get('receiver_client_id') else RECEIVER_CLIENT_IDS
        for receiver_client_id in target_receiver_ids:
            public_key_path = f"receiver_keys/ {receiver_client_id}_public.key"
            if not os.path.exists(public_key_path):
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Public key for {receiver_client_id} not found")
                continue
            with open(public_key_path, "r") as f:
                public_key_b64 = f.read().strip()
            encrypted_message = encrypt_message(message['message'], public_key_b64, receiver_client_id)
            if encrypted_message and encrypted_message['message_id'] not in message_ids:
                encrypted_messages.append(encrypted_message)
                message_ids.add(encrypted_message['message_id'])
                pending_messages[encrypted_message['message_id']] = encrypted_message
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Encrypted message {encrypted_message['message_id']} for {receiver_client_id}")
    return encrypted_messages

# Sends a batch of messages with retries using publish_batch
async def send_message_batch(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, messages: list):
    if not messages:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] No messages to send")
        return
    max_retries = 3
    timeout = 10
    message_ids = {msg['message_id']: msg for msg in messages}
    failed_messages = []
    for attempt in range(max_retries):
        try:
            messages_str = json.dumps(messages, ensure_ascii=False)
            command = f"publish_batch {EXCHANGE_NAME} {ROUTING_KEY} {messages_str}\n"
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Sending command: {command[:100]}...")
            writer.write(command.encode('utf-8'))
            await writer.drain()
            received_acks = set()
            start_time = time.time()
            while len(received_acks) < len(messages) and time.time() - start_time < timeout:
                response = (await asyncio.wait_for(reader.readline(), timeout=1)).decode('utf-8').strip()
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Server response: {response}")
                if response.startswith("ACK "):
                    message_id = response.split(" ", 2)[1]
                    if message_id in message_ids:
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Server ACK received for message {message_id}")
                        received_acks.add(message_id)
                        pending_messages.pop(message_id, None)
                    else:
                        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Unexpected ACK for message {message_id}")
                elif response.startswith("Error:"):
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Server error: {response}")
                    if "for message" in response:
                        message_id = response.split("for message ", 1)[1].strip()
                        if message_id in message_ids:
                            failed_messages.append(message_ids[message_id])
                            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Message {message_id} failed and will be retried")
                elif response.startswith("Invalid batch format"):
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Invalid batch format: {response}")
                    failed_messages = messages
                    break
                else:
                    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Unexpected server response: {response}")
            if len(received_acks) == len(messages):
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] All messages in batch acknowledged: {list(message_ids.keys())}")
                return
            else:
                failed_messages = [message_ids[mid] for mid in message_ids if mid not in received_acks]
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Missing ACKs for messages: {[mid for mid in message_ids if mid not in received_acks]}, retrying ({attempt + 1}/{max_retries})")
        except asyncio.TimeoutError:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Timeout waiting for server response, retrying ({attempt + 1}/{max_retries})")
            failed_messages = messages
        except Exception as e:
            print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Error sending batch: {e}")
            failed_messages = messages
        messages = failed_messages
        failed_messages = []
        await asyncio.sleep(2 ** attempt)
    if messages:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Failed to send messages after {max_retries} attempts: {[msg['message_id'] for msg in messages]}")

# Fetch public keys for all receivers
async def fetch_all_public_keys():
    public_keys = {}
    try:
        reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] TLS connection established for fetching public keys. Cipher: {writer.get_extra_info('cipher')}")
        for receiver_client_id in RECEIVER_CLIENT_IDS:
            public_key = await get_public_key(reader, writer, receiver_client_id)
            if public_key:
                public_keys[receiver_client_id] = public_key
            else:
                print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Skipping {receiver_client_id} due to missing public key")
        writer.close()
        await writer.wait_closed()
    except Exception as e:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Failed to fetch public keys: {e}")
    if not public_keys:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] No valid public keys fetched. Exiting")
        return {}
    return public_keys

# Sends generated messages
async def send_generated_messages(count: int = 20, send_batch_size: int = 10):
    public_keys = await fetch_all_public_keys()
    if not public_keys:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] No valid public keys available. Exiting")
        return
    global RECEIVER_CLIENT_IDS
    RECEIVER_CLIENT_IDS = list(public_keys.keys())
    print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Updated RECEIVER_CLIENT_IDS: {RECEIVER_CLIENT_IDS}")
    messages = generate_message_batch(count)
    encrypted_messages = await encrypt_message_batch(messages)
    reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
    try:
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] TLS connection established for sending messages. Cipher: {writer.get_extra_info('cipher')}")
        for i in range(0, len(encrypted_messages), send_batch_size):
            batch = encrypted_messages[i:i + send_batch_size]
            await send_message_batch(reader, writer, batch)
            await asyncio.sleep(0.1)  # Small delay to allow receiver to process ACKs
    finally:
        writer.close()
        await writer.wait_closed()
        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] [SENDER] Connection closed")

async def main():
    message_count = 10 
    await send_generated_messages(message_count)

if __name__ == '__main__':
    asyncio.run(main())
