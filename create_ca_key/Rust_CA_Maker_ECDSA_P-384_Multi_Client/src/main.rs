use rcgen::{CertificateParams, KeyPair, SanType};
use std::env;
use std::fs::{self, create_dir_all};
use time::OffsetDateTime;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <client_common_name1> [client_common_name2 ...]", args[0]);
        std::process::exit(1);
    }
    let client_cns = &args[1..];

    create_dir_all("certs")?;
    create_dir_all("src/client/certs")?;

    let ca_key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.not_before = OffsetDateTime::now_utc();
    ca_params.not_after = ca_params.not_before + time::Duration::days(3650);
    ca_params.distinguished_name.push(rcgen::DnType::CommonName, "CipherMQ");
    let ca_cert = ca_params.self_signed(&ca_key_pair)?;

    fs::write("certs/ca.key", ca_key_pair.serialize_pem())?;
    fs::write("certs/ca.crt", ca_cert.pem())?;

    let server_key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)?;
    let mut server_params = CertificateParams::default();
    server_params.not_before = OffsetDateTime::now_utc();
    server_params.not_after = server_params.not_before + time::Duration::days(365);
    server_params.distinguished_name.push(rcgen::DnType::CommonName, "localhost");
    server_params.subject_alt_names = vec![SanType::DnsName("localhost".try_into().unwrap())];
    let server_cert = server_params.signed_by(&server_key_pair, &ca_cert, &ca_key_pair)?;

    fs::write("certs/server.key", server_key_pair.serialize_pem())?;
    fs::write("certs/server.crt", server_cert.pem())?;

    for client_cn in client_cns {
        let client_dir = format!("certs/clients/{}", client_cn);
        create_dir_all(&client_dir)?;

        let client_key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)?;
        let mut client_params = CertificateParams::default();
        client_params.not_before = OffsetDateTime::now_utc();
        client_params.not_after = client_params.not_before + time::Duration::days(365);
        client_params.distinguished_name.push(rcgen::DnType::CommonName, client_cn.clone());
        let client_cert = client_params.signed_by(&client_key_pair, &ca_cert, &ca_key_pair)?;

        fs::write(Path::new(&client_dir).join("client.key"), client_key_pair.serialize_pem())?;
        fs::write(Path::new(&client_dir).join("client.crt"), client_cert.pem())?;

        let client_dest_dir = format!("src/client/certs/{}", client_cn);
        create_dir_all(&client_dest_dir)?;
        fs::copy("certs/ca.crt", Path::new(&client_dest_dir).join("ca.crt"))?;
        fs::copy(Path::new(&client_dir).join("client.crt"), Path::new(&client_dest_dir).join("client.crt"))?;
        fs::copy(Path::new(&client_dir).join("client.key"), Path::new(&client_dest_dir).join("client.key"))?;

        println!("Client certificate created with CN = '{}'", client_cn);
    }

    Ok(())
}