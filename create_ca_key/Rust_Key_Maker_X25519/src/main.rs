use x25519_dalek::{StaticSecret, PublicKey};
use rand_core::OsRng;
use base64::{engine::general_purpose, Engine as _};
use std::fs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let private_key = StaticSecret::new(&mut OsRng);
    let public_key = PublicKey::from(&private_key);

    let private_bytes = private_key.to_bytes();
    let public_bytes = public_key.as_bytes();

    let private_b64 = general_purpose::STANDARD.encode(private_bytes);
    let public_b64 = general_purpose::STANDARD.encode(public_bytes);

    fs::write("receiver_private.key", private_b64)?;
    fs::write("receiver_public.key", public_b64)?;

    println!("Keys saved successfully in Base64 format.");
    Ok(())
}
