use serde::Deserialize;
use std::fs;

#[derive(Deserialize)]
pub struct Config {
    pub connection_type: String, // "tcp" or "tls"
    pub address: String,
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let config_content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&config_content)?;
        Ok(config)
    }
}