# Generates a 2048-bit RSA key pair and saves the private and public keys to files for use in hybrid encryption
from Crypto.PublicKey import RSA

# Generates a 2048-bit RSA key pair using the PyCryptodome library
key = RSA.generate(1048)

# Saves the private key to a PEM file for use by the Receiver script
private_key = key.export_key()
with open("receiver_private.pem", "wb") as f:
    f.write(private_key)

# Saves the public key to a PEM file for use by the Sender script
public_key = key.publickey().export_key()
with open("receiver_public.pem", "wb") as f:
    f.write(public_key)