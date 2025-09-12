import json
import asyncio
import time
import ssl
import sys
import logging
from logging.handlers import RotatingFileHandler
from base64 import b64encode, b64decode
from cryptography import x509
from cryptography.hazmat.backends import default_backend
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from nacl.public import PrivateKey, PublicKey, SealedBox
import os
import uuid
from datetime import datetime

# Custom filter for logging levels
class LevelFilter(logging.Filter):
    def __init__(self, level):
        super().__init__()
        self.level = level

    def filter(self, record):
        return record.levelno == self.level

# Initialize logging
def setup_logging(config):
    logger = logging.getLogger('Sender')
    logger.setLevel(getattr(logging, config["logging"]["level"]))

    # Console handler
    console_handler = logging.StreamHandler(sys.stdout)
    console_handler.setFormatter(logging.Formatter(
        '%(asctime)s [%(levelname)s] %(message)s'
    ))
    logger.addHandler(console_handler)

    # File handlers for different log levels
    for level, file_path in [
        (logging.INFO, config["logging"]["info_file_path"]),
        (logging.DEBUG, config["logging"]["debug_file_path"]),
        (logging.ERROR, config["logging"]["error_file_path"])
    ]:
        handler = RotatingFileHandler(
            file_path,
            maxBytes=config["logging"]["max_size_mb"] * 1_000_000,
            backupCount=5
        )
        handler.setLevel(level)
        handler.addFilter(LevelFilter(level))
        handler.setFormatter(logging.Formatter(
            '{"time": "%(asctime)s", "level": "%(levelname)s", "message": "%(message)s"}'
        ))
        logger.addHandler(handler)

    return logger

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
        print(f"❌ [SENDER] Error extracting client_id from certificate: {e}")
        sys.exit(1)

# Load configuration
try:
    os.makedirs("logs", exist_ok=True)
    os.makedirs("keys", exist_ok=True)
    with open("config.json", "r") as config_file:
        config = json.load(config_file)
    EXCHANGE_NAME = config["exchange_name"]
    BINDINGS = config.get("bindings", [])
    SERVER_ADDRESS = config["server_address"]
    SERVER_PORT = config["server_port"]
    TLS_CONFIG = config["tls"]
    RECEIVER_CLIENT_IDS = config.get("receiver_client_ids", ["receiver_1", "receiver_2"])
    if isinstance(RECEIVER_CLIENT_IDS, str):
        RECEIVER_CLIENT_IDS = [RECEIVER_CLIENT_IDS]
    CLIENT_ID = extract_client_id(TLS_CONFIG)
    logger = setup_logging(config)
    logger.info(f"Extracted client_id from certificate: {CLIENT_ID}")
except FileNotFoundError:
    print("❌ [SENDER] Configuration file 'config.json' not found.")
    sys.exit(1)
except KeyError as e:
    print(f"❌ [SENDER] Missing key in configuration file: {e}")
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

# Configure server with queue, exchange, and bindings
async def configure_server(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    for binding in BINDINGS:
        queue_name = binding["queue_name"]
        exchange_name = binding["exchange_name"]
        routing_key = binding["routing_key"]

        # Declare queue
        command = f"declare_queue {queue_name}\n"
        logger.debug(f"Sending command: {command.strip()}")
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for queue declaration: {response}")

        # Declare exchange
        command = f"declare_exchange {exchange_name}\n"
        logger.debug(f"Sending command: {command.strip()}")
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for exchange declaration: {response}")

        # Bind queue to exchange
        command = f"bind {queue_name} {exchange_name} {routing_key}\n"
        logger.debug(f"Sending command: {command.strip()}")
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for binding: {response}")

# Get public key for a receiver from server
async def get_public_key(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, receiver_client_id: str) -> str | None:
    command = f"get_public_key {receiver_client_id}\n"
    logger.debug(f"Sending command: {command.strip()}")
    try:
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await asyncio.wait_for(reader.readline(), timeout=5)).decode('utf-8').strip()
        logger.debug(f"Server response: {response}")
        if response.startswith("Public key:"):
            public_key = response.split(" ", 2)[2]
            logger.info(f"Received public key for {receiver_client_id}")
            os.makedirs("keys", exist_ok=True)
            with open(f"keys/{receiver_client_id}_public.key", "w") as f:
                f.write(public_key)
            return public_key
        else:
            logger.error(f"Failed to get public key for {receiver_client_id}: {response}")
            return None
    except Exception as e:
        logger.error(f"Error fetching public key for {receiver_client_id}: {e}")
        return None

# Encrypt message using receiver's public key with hybrid encryption
def encrypt_message(message: str, public_key_b64: str, receiver_client_id: str) -> dict | None:
    try:
        # Load receiver's public key
        public_key_bytes = b64decode(public_key_b64)
        public_key = PublicKey(public_key_bytes)
        
        # Generate a random session key (32 bytes for ChaCha20Poly1305)
        session_key = os.urandom(32)
        
        # Encrypt session key with receiver's public key
        sealed_box = SealedBox(public_key)
        enc_session_key = sealed_box.encrypt(session_key)
        
        # Encrypt message with session key
        cipher = ChaCha20Poly1305(session_key)
        nonce = os.urandom(12)
        message_bytes = message.encode('utf-8')
        ciphertext = cipher.encrypt(nonce, message_bytes, None)
        
        # Extract tag and ciphertext
        tag = ciphertext[-16:]
        ciphertext = ciphertext[:-16]
        
        # Add sent_time to metadata
        sent_time = datetime.utcnow().isoformat() + 'Z'
        
        return {
            "message_id": str(uuid.uuid4()),
            "receiver_client_id": receiver_client_id,
            "enc_session_key": b64encode(enc_session_key).decode('utf-8'),
            "nonce": b64encode(nonce).decode('utf-8'),
            "tag": b64encode(tag).decode('utf-8'),
            "ciphertext": b64encode(ciphertext).decode('utf-8'),
            "sent_time": sent_time
        }
    except Exception as e:
        logger.error(f"Encryption failed for {receiver_client_id}: {e}")
        return None

# Generate a single message
def generate_message() -> dict:
    message = "Secure message from Sender to Receivers"
    return {"message": message}

# Encrypt message for all receivers with unique routing keys
async def encrypt_message_for_receivers(message: dict) -> list:
    encrypted_messages = []
    routing_keys = {binding["queue_name"]: binding["routing_key"] for binding in BINDINGS}
    
    for receiver_client_id in RECEIVER_CLIENT_IDS:
        public_key_path = f"keys/{receiver_client_id}_public.key"
        if not os.path.exists(public_key_path):
            logger.error(f"Public key for {receiver_client_id} not found")
            continue
        with open(public_key_path, "r") as f:
            public_key_b64 = f.read().strip()
        
        queue_name = f"receiver_{receiver_client_id.split('_')[-1]}_queue"
        routing_key = routing_keys.get(queue_name, f"receiver_{receiver_client_id.split('_')[-1]}.key")
        
        encrypted_message = encrypt_message(message['message'], public_key_b64, receiver_client_id)
        if encrypted_message:
            encrypted_message['routing_key'] = routing_key
            encrypted_messages.append(encrypted_message)
            pending_messages[encrypted_message['message_id']] = encrypted_message
            logger.info(f"Encrypted message {encrypted_message['message_id']} for {receiver_client_id} with routing_key {routing_key} and sent_time {encrypted_message['sent_time']}")
    return encrypted_messages

# Send a single message with retry
async def send_message(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, message: dict):
    max_retries = 3
    timeout = 10
    message_id = message['message_id']
    routing_key = message['routing_key']
    message_str = json.dumps(message, ensure_ascii=False)
    command = f"publish {EXCHANGE_NAME} {routing_key} {message_str}\n"
    
    for attempt in range(max_retries):
        try:
            logger.debug(f"Sending command: {command[:100]}... (Attempt {attempt + 1}/{max_retries})")
            writer.write(command.encode('utf-8'))
            await writer.drain()
            start_time = time.time()
            while time.time() - start_time < timeout:
                response = (await asyncio.wait_for(reader.readline(), timeout=1)).decode('utf-8').strip()
                logger.debug(f"Server response: {response}")
                if response == f"ACK {message_id}":
                    logger.info(f"Server ACK received for message {message_id}")
                    pending_messages.pop(message_id, None)
                    return True
                elif response.startswith("Error:"):
                    logger.error(f"Server error: {response}")
                    break
            logger.warning(f"No ACK received for message {message_id}, retrying ({attempt + 1}/{max_retries})")
        except asyncio.TimeoutError:
            logger.warning(f"Timeout waiting for server response, retrying ({attempt + 1}/{max_retries})")
        except Exception as e:
            logger.error(f"Error sending message {message_id}: {e}")
        await asyncio.sleep(2 ** attempt)
    logger.error(f"Failed to send message {message_id} after {max_retries} attempts")
    return False

# Fetch public keys for all receivers
async def fetch_all_public_keys():
    public_keys = {}
    try:
        reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
        logger.info(f"TLS connection established for fetching public keys. Cipher: {writer.get_extra_info('cipher')}")
        await configure_server(reader, writer)
        for receiver_client_id in RECEIVER_CLIENT_IDS:
            public_key = await get_public_key(reader, writer, receiver_client_id)
            if public_key:
                public_keys[receiver_client_id] = public_key
            else:
                logger.warning(f"Skipping {receiver_client_id} due to missing public key")
        writer.close()
        await writer.wait_closed()
    except Exception as e:
        logger.error(f"Failed to fetch public keys: {e}")
    if not public_keys:
        logger.error("No valid public keys fetched. Exiting")
        return {}
    return public_keys

# Main function to send a message
async def send_message_to_receivers():
    public_keys = await fetch_all_public_keys()
    if not public_keys:
        logger.error("No valid public keys available. Exiting")
        return

    global RECEIVER_CLIENT_IDS
    RECEIVER_CLIENT_IDS = list(public_keys.keys())
    logger.info(f"Updated RECEIVER_CLIENT_IDS: {RECEIVER_CLIENT_IDS}")

    message = generate_message()
    encrypted_messages = await encrypt_message_for_receivers(message)

    reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
    try:
        logger.info(f"TLS connection established for sending messages. Cipher: {writer.get_extra_info('cipher')}")
        await configure_server(reader, writer)
        for encrypted_message in encrypted_messages:
            await send_message(reader, writer, encrypted_message)
    finally:
        writer.close()
        await writer.wait_closed()
        logger.debug("Connection closed")

async def main():
    await send_message_to_receivers()

if __name__ == '__main__':
    asyncio.run(main())