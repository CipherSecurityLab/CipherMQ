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
from datetime import datetime, timezone

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
    RECEIVER_CLIENT_IDS = config.get("receiver_client_ids", ["receiver_1"])
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
ssl_context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
ssl_context.set_ciphers('ECDHE+AESGCM:ECDHE+CHACHA20:DHE+AESGCM:DHE+CHACHA20')
ssl_context.load_verify_locations(TLS_CONFIG["certificate_path"])
ssl_context.load_cert_chain(
    certfile=TLS_CONFIG["client_cert_path"],
    keyfile=TLS_CONFIG["client_key_path"]
)
ssl_context.verify_mode = getattr(ssl, TLS_CONFIG["verify_mode"])
ssl_context.check_hostname = TLS_CONFIG["check_hostname"]

# Generate a message
def generate_message():
    correlation_id = str(uuid.uuid4())[:8]
    current_time = datetime.now(timezone.utc).timestamp(),
    return {
        "correlation_id": correlation_id,
        "sender_id": CLIENT_ID,
        "sent_timestamp": current_time,
        "content": f"{CLIENT_ID}-CipherMQ Sample message with ID: {correlation_id}",
    }

# Encrypt message for a receiver
def encrypt_message(message, public_key_b64, receiver_client_id):
    try:
        # 1. Decode public key
        public_key = PublicKey(b64decode(public_key_b64))
        sealed_box = SealedBox(public_key)

        # 2. Generate session key and nonce
        session_key = os.urandom(32)  # 32-byte session key
        nonce = os.urandom(12)  # 12-byte nonce

        # 3. Encrypt session key with sealed box
        enc_session_key = sealed_box.encrypt(session_key)

        # 4. Encrypt message with ChaCha20Poly1305
        cipher = ChaCha20Poly1305(session_key)
        message_bytes = message["content"].encode('utf-8')
        ciphertext_with_tag = cipher.encrypt(nonce, message_bytes, None)

        # 5. Generate message ID and timestamp
        message_id = f"{message['sender_id']}-{message['correlation_id']}-{receiver_client_id}"
        sent_time = datetime.now(timezone.utc).isoformat()

        # 6. Construct encrypted message
        encrypted_message = {
            "message_id": message_id,
            "receiver_client_id": receiver_client_id,
            "enc_session_key": b64encode(enc_session_key).decode('utf-8'),
            "nonce": b64encode(nonce).decode('utf-8'),
            "ciphertext": b64encode(ciphertext_with_tag).decode('utf-8'),
            "sent_time": sent_time
        }

        logger.debug(f"Hybrid encryption completed for {receiver_client_id}: "
                     f"content_size={len(ciphertext_with_tag)}, "
                     f"session_key_size={len(session_key)}")
        return encrypted_message

    except Exception as e:
        logger.error(f"Encryption failed for {receiver_client_id}: {e}")
        return None

# Encrypt message for all receivers
async def encrypt_message_for_receivers(message: dict, receiver_client_ids: list):
    encrypted_messages = []
    routing_keys = {binding["queue_name"]: binding["routing_key"] for binding in BINDINGS}
    for receiver_client_id in receiver_client_ids:
        public_key_path = f"keys/{receiver_client_id}_public.key"
        if not os.path.exists(public_key_path):
            logger.error(f"Public key for {receiver_client_id} not found")
            continue
        with open(public_key_path, "r") as f:
            public_key_b64 = f.read().strip()
        queue_suffix = receiver_client_id
        queue_name = f"{queue_suffix}_queue"
        routing_key = routing_keys.get(queue_name, f"{queue_suffix}_key")
        encrypted_message = encrypt_message(message, public_key_b64, receiver_client_id)
        if encrypted_message:
            encrypted_message['routing_key'] = routing_key
            encrypted_messages.append(encrypted_message)
            pending_messages[encrypted_message['message_id']] = encrypted_message
            logger.info(f"Encrypted message {encrypted_message['message_id']} for {receiver_client_id} with routing_key {routing_key}")
    return encrypted_messages

# Configure server with queue, exchange, and bindings
async def configure_server(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    for binding in BINDINGS:
        queue_name = binding["queue_name"]
        exchange_name = binding["exchange_name"]
        routing_key = binding["routing_key"]
        command = f"declare_queue {queue_name}\n"
        logger.debug(f"Sending command: {command.strip()}")
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for queue declaration: {response}")
        command = f"declare_exchange {exchange_name}\n"
        logger.debug(f"Sending command: {command.strip()}")
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for exchange declaration: {response}")
        command = f"bind {queue_name} {exchange_name} {routing_key}\n"
        logger.debug(f"Sending command: {command.strip()}")
        writer.write(command.encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for binding: {response}")

# Get public key from server
async def get_public_key(reader, writer, client_id):
    command = f"get_public_key {client_id}\n"
    logger.debug(f"Sending command: {command.strip()}")
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    if response.startswith("Public key: "):
        return response.split("Public key: ", 1)[1]
    elif response == "Public key not found":
        logger.warning(f"Public key not found for {client_id}")
        return None
    else:
        logger.error(f"Error getting public key: {response}")
        return None

# Fetch public keys for all receivers and save to files
async def fetch_all_public_keys():
    public_keys = {}
    max_retries = 3
    for attempt in range(max_retries):
        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost"),
                timeout=120.0
            )
            logger.info(f"TLS connection established for fetching public keys. Cipher: {writer.get_extra_info('cipher')}")
            await configure_server(reader, writer)
            for receiver_client_id in RECEIVER_CLIENT_IDS:
                public_key = await get_public_key(reader, writer, receiver_client_id)
                if public_key:
                    public_keys[receiver_client_id] = public_key
                    public_key_path = f"keys/{receiver_client_id}_public.key"
                    try:
                        with open(public_key_path, "w") as f:
                            f.write(public_key)
                        logger.info(f"Saved public key for {receiver_client_id} to {public_key_path}")
                    except Exception as e:
                        logger.error(f"Failed to save public key for {receiver_client_id}: {e}")
                else:
                    logger.warning(f"Skipping {receiver_client_id} due to missing public key")
            writer.close()
            await writer.wait_closed()
            return public_keys
        except asyncio.TimeoutError:
            logger.warning(f"Timeout on attempt {attempt + 1}/{max_retries}, retrying in {2 ** attempt} seconds")
            await asyncio.sleep(2 ** attempt)
        except Exception as e:
            logger.error(f"Failed to fetch public keys on attempt {attempt + 1}/{max_retries}: {e}")
            await asyncio.sleep(2 ** attempt)
    logger.error("No valid public keys fetched after all retries")
    return public_keys

# Send multiple messages
async def send_message_to_receivers(num_messages=5):
    public_keys = await fetch_all_public_keys()
    receiver_client_ids = list(public_keys.keys())
    if not public_keys:
        logger.warning("No public keys fetched from server. Attempting to use local keys")
        public_keys = {}
        for receiver_client_id in RECEIVER_CLIENT_IDS:
            public_key_path = f"keys/{receiver_client_id}_public.key"
            if os.path.exists(public_key_path):
                with open(public_key_path, "r") as f:
                    public_keys[receiver_client_id] = f.read().strip()
        receiver_client_ids = list(public_keys.keys())
        if not public_keys:
            logger.error("No valid public keys available (local or server). Exiting")
            return

    logger.info(f"Using receiver_client_ids: {receiver_client_ids}")

    reader, writer = await asyncio.wait_for(
        asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost"),
        timeout=120.0
    )
    start_time = time.time()
    try:
        logger.info(f"TLS connection established for sending messages. Cipher: {writer.get_extra_info('cipher')}")
        await configure_server(reader, writer)
        for i in range(num_messages):
            message = generate_message()
            encrypted_messages = await encrypt_message_for_receivers(message, receiver_client_ids)
            for encrypted_message in encrypted_messages:
                await send_message(reader, writer, encrypted_message)
            logger.info(f"Sent message {i+1}/{num_messages}")
        end_time = time.time()
        logger.info(f"Sent {num_messages} messages in {end_time - start_time:.2f} seconds")
        logger.info(f"Throughput: {num_messages / (end_time - start_time):.2f} messages/second")
    except asyncio.TimeoutError:
        logger.error("Timeout while sending messages")
    finally:
        writer.close()
        await writer.wait_closed()
        logger.debug("Connection closed")

async def check_server_health():
    try:
        reader, writer = await asyncio.wait_for(
            asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost"),
            timeout=10.0
        )
        writer.close()
        await writer.wait_closed()
        return True
    except Exception as e:
        logger.error(f"Server health check failed: {e}")
        return False

# Send a single message with retry
async def send_message(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, message: dict):
    max_retries = 3
    timeout = 30
    message_id = message['message_id']
    routing_key = message['routing_key']
    message_str = json.dumps({
        "message_id": message_id,
        "ciphertext": message['ciphertext'],
        "receiver_client_id": message['receiver_client_id'],
        "enc_session_key": message['enc_session_key'],
        "nonce": message['nonce'],
        "sent_time": message['sent_time']
    }, ensure_ascii=False)
    command = f"publish {EXCHANGE_NAME} {routing_key} {message_str}\n"
    for attempt in range(max_retries):
        try:
            logger.debug(f"Sending message {message_id} (Attempt {attempt + 1}/{max_retries})")
            writer.write(command.encode('utf-8'))
            await writer.drain()
            response = (await asyncio.wait_for(reader.readline(), timeout=timeout)).decode('utf-8').strip()
            logger.debug(f"Received response: {response}")
            if response == f"ACK {message_id}":
                logger.info(f"ACK received for message {message_id}")
                pending_messages.pop(message_id, None)
                return True
            elif response.startswith("Error:"):
                logger.error(f"Server error for message {message_id}: {response}")
                return False
            else:
                logger.warning(f"Unexpected response for message {message_id}: {response}")
        except asyncio.TimeoutError:
            logger.warning(f"Timeout for message {message_id}, retrying ({attempt + 1}/{max_retries})")
        except Exception as e:
            logger.error(f"Error sending message {message_id}: {e}")
            return False
        await asyncio.sleep(2 ** attempt)
    logger.error(f"Failed to send message {message_id} after {max_retries} attempts")
    return False

# Send batch of messages
async def send_messages_persistent(num_messages=100):
    public_keys = await fetch_all_public_keys()
    receiver_client_ids = list(public_keys.keys())
    if not public_keys:
        logger.error("No valid public keys available. Exiting")
        return

    logger.info(f"Using receiver_client_ids: {receiver_client_ids}")

    reader, writer = await asyncio.wait_for(
        asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost"),
        timeout=120.0
    )

    try:
        logger.info(f"TLS connection established. Cipher: {writer.get_extra_info('cipher')}")
        await configure_server(reader, writer)

        batch_size = 100
        delay_between_batches = 0.1
        failed_messages = []

        for batch_start in range(0, num_messages, batch_size):
            batch_end = min(batch_start + batch_size, num_messages)
            logger.info(f"Sending batch {batch_start//batch_size + 1}: messages {batch_start+1}-{batch_end}")

            for i in range(batch_start, batch_end):
                message = generate_message()
                encrypted_messages = await encrypt_message_for_receivers(message, receiver_client_ids)

                for encrypted_message in encrypted_messages:
                    success = await send_message(reader, writer, encrypted_message)
                    if not success:
                        logger.warning(f"Message {encrypted_message['message_id']} failed, adding to retry queue")
                        failed_messages.append(encrypted_message)

                await asyncio.sleep(0.1)

            if batch_end < num_messages:
                logger.debug(f"Batch completed, waiting {delay_between_batches}s before next batch")
                await asyncio.sleep(delay_between_batches)

        if failed_messages:
            logger.info(f"Retrying {len(failed_messages)} failed messages")
            for encrypted_message in failed_messages:
                await send_message(reader, writer, encrypted_message)
                await asyncio.sleep(0.1)

        logger.info(f"Successfully sent {num_messages} messages (with {len(failed_messages)} retries)")

    finally:
        writer.close()
        await writer.wait_closed()
        logger.info("Connection closed after sending messages")

async def main():
    logger.info("Starting sender")
    await send_messages_persistent(num_messages=100)

if __name__ == '__main__':
    asyncio.run(main())