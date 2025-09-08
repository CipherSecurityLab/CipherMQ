import asyncio
import json
import signal
import ssl
import sys
import time
import logging
from logging.handlers import RotatingFileHandler
from base64 import b64decode, b64encode
from cryptography.hazmat.primitives.asymmetric import x25519
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives import serialization
from nacl.public import PrivateKey, SealedBox
import os

# Custom filter for logging levels
class LevelFilter(logging.Filter):
    def __init__(self, level):
        super().__init__()
        self.level = level

    def filter(self, record):
        return record.levelno == self.level

# Initialize logging
def setup_logging(config):
    logger = logging.getLogger('Receiver')
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

# Load configuration
try:
    os.makedirs("logs", exist_ok=True)
    os.makedirs("keys", exist_ok=True)
    with open("config.json", "r") as config_file:
        config = json.load(config_file)
    QUEUE_NAME = config["queue_name"]
    EXCHANGE_NAME = config["exchange_name"]
    ROUTING_KEY = config["routing_key"]
    SERVER_ADDRESS = config["server_address"]
    SERVER_PORT = config["server_port"]
    TLS_CONFIG = config["tls"]
    logger = setup_logging(config)
except FileNotFoundError:
    print("❌ [RECEIVER] Configuration file 'config.json' not found.")
    sys.exit(1)
except KeyError as e:
    print(f"❌ [RECEIVER] Missing key in configuration file: {e}")
    sys.exit(1)

# Thread-safe queue for processed messages
message_queue = asyncio.Queue()
running = True
processed_messages = set()

# Load private and public keys
try:
    with open("keys/receiver_private.key", "r") as key_file:
        private_key_bytes = b64decode(key_file.read())
        PRIVATE_KEY = PrivateKey(private_key_bytes)
    with open("keys/receiver_public.key", "r") as key_file:
        PUBLIC_KEY = x25519.X25519PublicKey.from_public_bytes(b64decode(key_file.read()))
except Exception as e:
    logger.error(f"Error loading keys: {e}")
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
async def register_public_key(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> bool:
    public_key_b64 = b64encode(PUBLIC_KEY.public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw
    )).decode('utf-8')
    command = f"register_public_key {public_key_b64}\n"
    logger.debug(f"Sending command: {command.strip()}")
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for public key registration: {response}")
    return response == "Public key registered"

# Configure server (declare queue, exchange, and bind)
async def configure_server(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    # Declare queue
    command = f"declare_queue {QUEUE_NAME}\n"
    logger.debug(f"Sending command: {command.strip()}")
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for queue declaration: {response}")

    # Declare exchange
    command = f"declare_exchange {EXCHANGE_NAME}\n"
    logger.debug(f"Sending command: {command.strip()}")
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for exchange declaration: {response}")

    # Bind queue to exchange
    command = f"bind {QUEUE_NAME} {EXCHANGE_NAME} {ROUTING_KEY}\n"
    logger.debug(f"Sending command: {command.strip()}")
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for binding: {response}")

# Decrypt message using hybrid encryption
async def decrypt_message_hybrid(data: dict) -> str | None:
    try:
        required_keys = {"enc_session_key", "nonce", "tag", "ciphertext", "message_id"}
        if not required_keys.issubset(data.keys()):
            logger.error(f"Incomplete message for hybrid decryption: {data}")
            return None

        enc_session_key = b64decode(data['enc_session_key'])
        nonce = b64decode(data['nonce'])
        tag = b64decode(data['tag'])
        ciphertext = b64decode(data['ciphertext'])

        # Decrypt session key with receiver's private key
        sealed_box = SealedBox(PRIVATE_KEY)
        session_key = sealed_box.decrypt(enc_session_key)
        
        # Decrypt message with session key
        cipher = ChaCha20Poly1305(session_key)
        encrypted_bytes = ciphertext + tag
        plaintext = cipher.decrypt(nonce, encrypted_bytes, None)
        return plaintext.decode('utf-8')
    except Exception as e:
        logger.error(f"Error in hybrid decryption: {e}")
        return None

# Send ACK with retry mechanism
async def send_ack_with_retry(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, message_id: str) -> bool:
    max_retries = 3
    timeout = 10
    for attempt in range(max_retries):
        try:
            logger.info(f"Sending ACK for message {message_id} (Attempt {attempt + 1}/{max_retries})")
            writer.write(f"ack {message_id}\n".encode('utf-8'))
            await writer.drain()
            start_time = time.time()
            while time.time() - start_time < timeout:
                try:
                    response = (await asyncio.wait_for(reader.readline(), timeout=2.0)).decode('utf-8').strip()
                    logger.debug(f"ACK response: {response}")
                    if response.startswith(f"ACK_CONFIRMED {message_id}"):
                        logger.info(f"Server confirmed ACK for message {message_id}")
                        return True
                except asyncio.TimeoutError:
                    continue
            logger.warning(f"No correct ACK received, retrying ({attempt + 1}/{max_retries})")
        except Exception as e:
            logger.error(f"Error sending ACK for message {message_id}: {e}")
        await asyncio.sleep(2 ** attempt)
    logger.error(f"Failed to acknowledge message {message_id}")
    return False

# Process a single message
async def process_message(message: str, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    if not message.startswith("Message:"):
        logger.warning(f"Unhandled message: {message}")
        return
    try:
        parts = message.split(" ", 2)
        if len(parts) < 3:
            logger.error("Invalid message format")
            return
        message_id = parts[1]
        if message_id in processed_messages:
            logger.warning(f"Duplicate message {message_id}, sending ACK")
            await send_ack_with_retry(reader, writer, message_id)
            return
        message_data = json.loads(parts[2])
        logger.info(f"Processing message {message_id}")
        decrypted = await decrypt_message_hybrid(message_data)
        if decrypted:
            logger.info(f"Decrypted message {message_id}: {decrypted}")
            await message_queue.put((message_id, decrypted))
            processed_messages.add(message_id)
            await send_ack_with_retry(reader, writer, message_id)
        else:
            logger.error(f"Decryption failed for message {message_id}")
    except json.JSONDecodeError as e:
        logger.error(f"JSON error in message parsing: {e}")
    except Exception as e:
        logger.error(f"Error processing message: {e}")

# Receive and process messages
async def receive_messages(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    global running
    try:
        if not await register_public_key(reader, writer):
            logger.error("Public key registration failed. Exiting")
            return

        await configure_server(reader, writer)

        logger.info(f"Subscribing to queue {QUEUE_NAME}")
        writer.write(f"consume {QUEUE_NAME}\n".encode('utf-8'))
        await writer.drain()
        response = (await reader.readline()).decode('utf-8').strip()
        logger.info(f"Server response for subscription: {response}")

        while running:
            try:
                line = await asyncio.wait_for(reader.readline(), timeout=1.0)
                if not line:
                    logger.error("Connection closed")
                    break
                message = line.decode('utf-8').strip()
                if not message:
                    continue
                logger.debug(f"Raw message received: {message}")
                await process_message(message, reader, writer)
            except asyncio.TimeoutError:
                continue
            except Exception as e:
                logger.error(f"Error receiving messages: {e}")
                break
    finally:
        writer.close()
        await writer.wait_closed()
        logger.debug("Connection closed")

# Process decrypted messages with batching
async def process_messages():
    global running
    logger.info("Starting message processing")
    batch_size = 10
    batch = []
    output_file = f"data/{QUEUE_NAME}_received_messages.jsonl"
    try:
        os.makedirs(os.path.dirname(output_file) or ".", exist_ok=True)
        with open(output_file, "a", encoding='utf-8') as f:
            while running:
                try:
                    message_id, message = await asyncio.wait_for(message_queue.get(), timeout=1.0)
                    logger.info(f"Message read from queue: {message_id}")
                    batch.append({"message_id": message_id, "message": message, "timestamp": time.time()})
                    if len(batch) >= batch_size:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        logger.info(f"Batch of {len(batch)} messages saved to {output_file}")
                        batch.clear()
                    message_queue.task_done()
                except asyncio.TimeoutError:
                    if batch:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        logger.info(f"Batch of {len(batch)} messages saved to {output_file}")
                        batch.clear()
                except Exception as e:
                    logger.error(f"Error saving message to {output_file}: {e}")
    except Exception as e:
        logger.error(f"Error opening/writing file {output_file}: {e}")
    finally:
        if batch:
            try:
                with open(output_file, "a", encoding='utf-8') as f:
                    for item in batch:
                        json.dump(item, f, ensure_ascii=False)
                        f.write("\n")
                    f.flush()
                    logger.info(f"Final batch of {len(batch)} messages saved to {output_file}")
            except Exception as e:
                logger.error(f"Error saving final batch to {output_file}: {e}")
        logger.info("Message processing stopped")

# Handle SIGINT for graceful shutdown
def signal_handler(loop):
    global running
    logger.info("Received SIGINT, shutting down")
    running = False
    loop.stop()
    loop.run_until_complete(loop.shutdown_asyncgens())
    loop.close()
    sys.exit(0)

# Main function with reconnection logic
async def main():
    loop = asyncio.get_running_loop()
    signal.signal(signal.SIGINT, lambda s, f: signal_handler(loop))
    logger.info("Starting message processing task")
    asyncio.create_task(process_messages())

    while running:
        try:
            reader, writer = await asyncio.open_connection(SERVER_ADDRESS, SERVER_PORT, ssl=ssl_context, server_hostname="localhost")
            logger.info(f"TLS connection established. Cipher: {writer.get_extra_info('cipher')}")
            await receive_messages(reader, writer)
        except Exception as e:
            logger.error(f"Connection failed: {e}. Retrying in 1 second")
            await asyncio.sleep(1)

if __name__ == "__main__":
    asyncio.run(main())