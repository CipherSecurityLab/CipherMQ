from nacl.public import PrivateKey
from base64 import b64encode
import os

certs_dir = "receiver/certs"

os.makedirs(certs_dir, exist_ok=True)

private_key = PrivateKey.generate()
public_key = private_key.public_key

with open(os.path.join(certs_dir, "receiver_private.key"), "w") as f:
    f.write(b64encode(bytes(private_key)).decode("utf-8"))

with open(os.path.join(certs_dir, "receiver_public.key"), "w") as f:
    f.write(b64encode(bytes(public_key)).decode("utf-8"))

print("x25519 key pair generated in certs/")
