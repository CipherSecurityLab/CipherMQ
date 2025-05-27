import json
import socket
from base64 import b64encode
from Crypto.Cipher import AES, PKCS1_OAEP
from Crypto.PublicKey import RSA
from Crypto.Random import get_random_bytes
import uuid

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
        return {
            'message_id': str(uuid.uuid4()),  # Add message_id
            'enc_session_key': b64encode(enc_session_key).decode('utf-8'),
            'nonce': b64encode(cipher_aes.nonce).decode('utf-8'),
            'tag': b64encode(tag).decode('utf-8'),
            'ciphertext': b64encode(ciphertext).decode('utf-8')
        }
    except Exception as e:
        print(f"❌ Error in hybrid encryption: {e}")
        return None

# Sends a publish command with the encrypted message as JSON to the Rust message broker
def send_message(message: str, public_key_path: str, exchange: str = "default_exchange", routing_key: str = "default_key"):
    encrypted_data = encrypt_message_hybrid(message, public_key_path)
    if not encrypted_data:
        print("❌ Encryption failed.")
        return

    print("📦 Encrypted data:")
    print(json.dumps(encrypted_data, indent=2))

    # Send to server via TCP
    try:
        client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        client.connect(('127.0.0.1', 5672))
        
        # Send publish command
        command = f"publish {exchange} {routing_key} {json.dumps(encrypted_data)}\n"
        client.send(command.encode('utf-8'))
        
        # Receive server response
        response = client.recv(1024).decode('utf-8').strip()
        print(f"✅ Server response: {response}")
        
        client.close()
    except Exception as e:
        print(f"❌ Error sending message: {e}")

def main():
    message = "This is a test hybrid message."
    print("📤 Original message:", message)
    send_message(message, 'receiver_public.pem')

if __name__ == '__main__':
    main()