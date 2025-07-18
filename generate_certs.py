import os
from cryptography import x509
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.hashes import SHA384
from cryptography.x509.oid import NameOID
from datetime import datetime, timedelta
from pathlib import Path

def generate_key_pair():
    """Generate an ECDSA P-384 key pair."""
    private_key = ec.generate_private_key(curve=ec.SECP384R1())
    public_key = private_key.public_key()
    return private_key, public_key

def create_ca_cert():
    """Create CA certificate with CN=CipherMQ-CA."""
    private_key, public_key = generate_key_pair()
    subject = issuer = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, "CipherMQ-CA")
    ])
    cert_builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(public_key)
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.utcnow())
        .not_valid_after(datetime.utcnow() + timedelta(days=3650))
        .add_extension(
            x509.BasicConstraints(ca=True, path_length=None), critical=True
        )
    )
    ca_cert = cert_builder.sign(private_key, SHA384())
    return private_key, ca_cert

def create_cert(subject_cn, ca_cert, ca_key, is_server=False):
    """Create a server or client certificate signed by the CA."""
    private_key, public_key = generate_key_pair()
    subject = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, subject_cn)
    ])
    cert_builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(ca_cert.subject)
        .public_key(public_key)
        .serial_number(x509.random_serial_number())
        .not_valid_before(datetime.utcnow())
        .not_valid_after(datetime.utcnow() + timedelta(days=365))
    )
    if is_server:
        cert_builder = cert_builder.add_extension(
            x509.SubjectAlternativeName([x509.DNSName("localhost")]),
            critical=False
        )
    cert = cert_builder.sign(ca_key, SHA384())
    return private_key, cert

def save_pem_file(path, data, is_private_key=False):
    """Save data to a PEM file with appropriate permissions."""
    mode = 0o600 if is_private_key else 0o644
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    os.chmod(path, mode)

def main():
    # Define directories
    certs_dir = Path("certs")
    sender_certs_dir = Path("src/client/sender/certs")
    receiver_certs_dir = Path("src/client/receiver/certs")

    # Create directories
    certs_dir.mkdir(exist_ok=True)
    sender_certs_dir.mkdir(parents=True, exist_ok=True)
    receiver_certs_dir.mkdir(parents=True, exist_ok=True)

    # Generate CA certificate
    ca_key, ca_cert = create_ca_cert()
    save_pem_file(certs_dir / "ca.key", ca_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption()
    ), is_private_key=True)
    save_pem_file(certs_dir / "ca.crt", ca_cert.public_bytes(serialization.Encoding.PEM))

    # Generate server certificate
    server_key, server_cert = create_cert("CipherMQ-Server", ca_cert, ca_key, is_server=True)
    save_pem_file(certs_dir / "server.key", server_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption()
    ), is_private_key=True)
    save_pem_file(certs_dir / "server.crt", server_cert.public_bytes(serialization.Encoding.PEM))

    # Generate client certificates for Sender and Receiver
    client_cns = [
        ("CipherMQ-Sender", sender_certs_dir),
        ("CipherMQ-Receiver", receiver_certs_dir)
    ]
    for cn, dest_dir in client_cns:
        client_key, client_cert = create_cert(cn, ca_cert, ca_key)
        save_pem_file(dest_dir / "client.key", client_key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.PKCS8,
            encryption_algorithm=serialization.NoEncryption()
        ), is_private_key=True)
        save_pem_file(dest_dir / "client.crt", client_cert.public_bytes(serialization.Encoding.PEM))
        save_pem_file(dest_dir / "ca.crt", ca_cert.public_bytes(serialization.Encoding.PEM))

        print(f"[{datetime.now():%Y-%m-%d %H:%M:%S}] Client certificate created with CN = '{cn}' in {dest_dir}")

if __name__ == "__main__":
    main()