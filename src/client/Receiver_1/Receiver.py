import asyncio
from datetime import datetime, timezone
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

    console_handler = logging.StreamHandler(sys.stdout)
    console_handler.setFormatter(logging.Formatter(
        '%(asctime)s [%(levelname)s] %(message)s'
    ))
    logger.addHandler(console_handler)

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
    os.makedirs("data", exist_ok=True)
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

# Global variables - queues will be initialized in async context
message_queue = None
ack_queue = None
running = True
processed_messages = set()

# Load keys
try:
    with open("../../../create_ca_key/Rust_Key_Maker_X25519/receiver_private.key", "r") as key_file:
        private_key_bytes = b64decode(key_file.read())
        PRIVATE_KEY = PrivateKey(private_key_bytes)
    with open("../../../create_ca_key/Rust_Key_Maker_X25519/receiver_public.key", "r") as key_file:
        PUBLIC_KEY_X25519 = x25519.X25519PublicKey.from_public_bytes(b64decode(key_file.read()))
except Exception as e:
    logger.error(f"Error loading keys: {e}")
    sys.exit(1)

# SSL context
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
    public_key_b64 = b64encode(PUBLIC_KEY_X25519.public_bytes(
        encoding=serialization.Encoding.Raw,
        format=serialization.PublicFormat.Raw
    )).decode('utf-8')
    command = f"register_public_key {public_key_b64}\n"
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for public key registration: {response}")
    return response == "Public key registered"

# Configure server (declare queue, exchange, and bind)
async def configure_server(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    command = f"declare_queue {QUEUE_NAME}\n"
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for queue declaration: {response}")

    command = f"declare_exchange {EXCHANGE_NAME}\n"
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for exchange declaration: {response}")

    command = f"bind {QUEUE_NAME} {EXCHANGE_NAME} {ROUTING_KEY}\n"
    writer.write(command.encode('utf-8'))
    await writer.drain()
    response = (await reader.readline()).decode('utf-8').strip()
    logger.info(f"Server response for binding: {response}")

async def ack_sender_worker(writer: asyncio.StreamWriter):
    global running, ack_queue
    while running:
        try:
            message_id = await asyncio.wait_for(ack_queue.get(), timeout=0.5)
            
            if writer.is_closing():
                logger.warning("Writer closed, stopping ACK sender")
                break
            
            command = f"ack {message_id}\n"
            writer.write(command.encode('utf-8'))
            await writer.drain()
            
            logger.debug(f"Sent ACK for {message_id}")
            ack_queue.task_done()
            
        except asyncio.TimeoutError:
            continue
        except Exception as e:
            logger.error(f"Error in ACK sender: {e}")
            await asyncio.sleep(0.1)

async def process_message(message: str):
    global message_queue, ack_queue
    message = message.strip()
    
    if not message or not message.startswith("Message:"):
        return
        
    try:
        parts = message[len("Message:"):].strip().split(' ', 1)
        if len(parts) < 2:
            logger.error(f"Invalid message format")
            return
            
        message_id, message_str = parts
        
        if message_id in processed_messages:
            logger.debug(f"Duplicate message {message_id}")
            return
            
        message_data = json.loads(message_str)
        
        # Extract components
        enc_session_key = b64decode(message_data["enc_session_key"])
        nonce = b64decode(message_data["nonce"])
        ciphertext_with_tag = b64decode(message_data["ciphertext"])
        
        # Decrypt session key
        sealed_box = SealedBox(PRIVATE_KEY)
        session_key = sealed_box.decrypt(enc_session_key)
        
        # Decrypt message
        cipher = ChaCha20Poly1305(session_key)
        plaintext = cipher.decrypt(nonce, ciphertext_with_tag, None)
        decrypted_message = plaintext.decode('utf-8')
        
        # Store decrypted message
        processed_messages.add(message_id)
        await message_queue.put((message_id, {"content": decrypted_message}))
        
        # Add ACK to queue
        await ack_queue.put(message_id)
        
        logger.info(f"Processed and decrypted message {message_id}")
            
    except Exception as e:
        logger.error(f"Error processing message {message_id}: {e}")

async def receive_messages(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    global running
    
    ack_sender_task = asyncio.create_task(ack_sender_worker(writer))
    
    try:
        if not await register_public_key(reader, writer):
            logger.error("Public key registration failed")
            return

        await configure_server(reader, writer)

        logger.info(f"Subscribing to queue {QUEUE_NAME}")
        writer.write(f"consume {QUEUE_NAME}\n".encode('utf-8'))
        await writer.drain()
        
        logger.info(f"Waiting for messages (high-throughput mode)")

        consecutive_empty = 0
        
        while running and not writer.is_closing():
            try:
                line = await asyncio.wait_for(reader.readline(), timeout=0.5)
                
                if not line:
                    logger.error("Connection closed")
                    break
                
                consecutive_empty = 0
                
                message = line.decode('utf-8').strip()
                if not message:
                    continue
                
                await process_message(message)
                
            except asyncio.TimeoutError:
                consecutive_empty += 1
                if consecutive_empty >= 120:
                    logger.debug("No messages for 60 seconds")
                    consecutive_empty = 0
                continue
                
            except Exception as e:
                logger.error(f"Error in receive loop: {e}")
                break
                
    finally:
        running = False
        ack_sender_task.cancel()
        try:
            writer.close()
            await writer.wait_closed()
        except:
            pass
        logger.info("Connection closed")

async def process_messages():
    global running, message_queue
    logger.info("Starting message processing")
    batch_size = 100
    batch = []
    output_file = f"data/{QUEUE_NAME}_received_messages.jsonl"
    start_time = time.time()
    message_count = 0
    
    try:
        os.makedirs(os.path.dirname(output_file) or ".", exist_ok=True)
        with open(output_file, "a", encoding='utf-8') as f:
            while running:
                try:
                    message_id, message = await asyncio.wait_for(message_queue.get(), timeout=5.0)
                    
                    batch.append({
                        "message_id": message_id, 
                        "message": message, 
                        "timestamp": datetime.now(timezone.utc).timestamp(),
                    })
                    message_count += 1
                    
                    if len(batch) >= batch_size:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        logger.info(f"Saved batch of {len(batch)} messages (total: {message_count})")
                        batch.clear()
                    
                    message_queue.task_done()
                    
                except asyncio.TimeoutError:
                    if batch:
                        for item in batch:
                            json.dump(item, f, ensure_ascii=False)
                            f.write("\n")
                        f.flush()
                        logger.info(f"Saved batch of {len(batch)} messages")
                        batch.clear()
                        
            if batch:
                for item in batch:
                    json.dump(item, f, ensure_ascii=False)
                    f.write("\n")
                f.flush()
                
    except Exception as e:
        logger.error(f"Error in message processing: {e}")
    finally:
        end_time = time.time()
        if message_count > 0:
            logger.info(f"Received {message_count} messages in {end_time - start_time:.2f} seconds")
            logger.info(f"Throughput: {message_count / (end_time - start_time):.2f} messages/second")
        logger.info("Message processing stopped")

def signal_handler(loop):
    global running
    logger.info("Received SIGINT, shutting down")
    running = False
    loop.call_later(1, loop.stop)

async def main():
    global message_queue, ack_queue
    
    # Initialize queues in async context
    message_queue = asyncio.Queue()
    ack_queue = asyncio.Queue()
    
    loop = asyncio.get_running_loop()
    signal.signal(signal.SIGINT, lambda s, f: signal_handler(loop))
    
    processing_task = asyncio.create_task(process_messages())

    while running:
        try:
            reader, writer = await asyncio.open_connection(
                SERVER_ADDRESS, SERVER_PORT, 
                ssl=ssl_context, 
                server_hostname="localhost"
            )
            logger.info(f"TLS connection established. Cipher: {writer.get_extra_info('cipher')}")
            await receive_messages(reader, writer)
        except Exception as e:
            logger.error(f"Connection failed: {e}")
        finally:
            if running:
                await asyncio.sleep(1)
    
    await processing_task

if __name__ == "__main__":
    asyncio.run(main())