import json
import socket
import time
from base64 import b64encode
from Crypto.Cipher import AES, PKCS1_OAEP
from Crypto.PublicKey import RSA
from Crypto.Random import get_random_bytes
import uuid

# Stores pending messages until acknowledged
pending_messages = {}

# Performs hybrid encryption: RSA encrypts a random session key, and AES-GCM encrypts the message
def encrypt_message_hybrid(message: str, public_key_path: str) -> dict:
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
            'ciphertext': b64encode(ciphertext).decode('utf-8')
        }
        pending_messages[message_id] = encrypted_data
        print(f"📤 [SENDER] Message {message_id} added to pending messages")
        return encrypted_data
    except Exception as e:
        print(f"❌ [SENDER] Error in hybrid encryption: {e}")
        return None

# Sends a publish command with retries until acknowledgment
def send_message(message: str, public_key_path: str, exchange: str = "default_exchange", routing_key: str = "default_key"):
    encrypted_data = encrypt_message_hybrid(message, public_key_path)
    if not encrypted_data:
        print("❌ [SENDER] Encryption failed.")
        return

    print(f"📦 [SENDER] Encrypted data for message {encrypted_data['message_id']}:\n{json.dumps(encrypted_data, indent=2)}")

    max_retries = 3
    timeout = 5  # seconds
    message_id = encrypted_data['message_id']
    
    for attempt in range(max_retries):
        try:
            client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            client.settimeout(timeout)
            client.connect(('127.0.0.1', 5672))
            
            # Send publish command
            command = f"publish {exchange} {routing_key} {json.dumps(encrypted_data)}\n"
            print(f"🚀 [SENDER] Sending message {message_id} to server (Attempt {attempt + 1}/{max_retries})")
            client.send(command.encode('utf-8'))
            
            # Wait for acknowledgment
            response = client.recv(1024).decode('utf-8').strip()
            if response.startswith(f"ACK {message_id}"):
                print(f"✅ [SENDER] Server ACK received for message {message_id}: {response}")
                pending_messages.pop(message_id, None)
                print(f"🗑️ [SENDER] Message {message_id} removed from pending messages")
                client.close()
                return
            print(f"⚠️ [SENDER] Unexpected server response: {response}")
            client.close()
        except socket.timeout:
            print(f"🔁 [SENDER] Timeout for message {message_id}, retrying ({attempt + 1}/{max_retries})...")
        except Exception as e:
            print(f"❌ [SENDER] Error sending message {message_id}: {e}")
        time.sleep(1)
    
    print(f"❌ [SENDER] Failed to send message {message_id} after {max_retries} attempts.")

def main():
    message = "This is a test hybrid message."
    print(f"📝 [SENDER] Original message: {message}")
    send_message(message, 'receiver_public.pem')

if __name__ == '__main__':
    main()