use serde::Deserialize;
use std::fs;
use toml;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    ReadError(#[from] std::io::Error),
    #[error("Failed to parse config file: {0}")]
    ParseError(#[from] toml::de::Error),
    #[error("Missing required field: {0}")]
    MissingField(String),
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServerConfig {
    pub address: String,
    pub connection_type: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    pub ca_cert_path: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DatabaseConfig {
    pub dbname: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EncryptionConfig {
    pub aes_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub database: DatabaseConfig,
    pub encryption: EncryptionConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        tracing::info!("Loading configuration from {}", path);
        let config_content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&config_content)?;
        
        if config.server.connection_type == "tls" {
            if config.tls.cert_path.is_none() {
                tracing::error!("Missing cert_path for TLS configuration");
                return Err(ConfigError::MissingField("tls.cert_path".to_string()));
            }
            if config.tls.key_path.is_none() {
                tracing::error!("Missing key_path for TLS configuration");
                return Err(ConfigError::MissingField("tls.key_path".to_string()));
            }
            if config.tls.ca_cert_path.is_none() {
                tracing::error!("Missing ca_cert_path for TLS configuration");
                return Err(ConfigError::MissingField("tls.ca_cert_path".to_string()));
            }
        }

        let config = Config {
            server: ServerConfig {
                address: config.server.address,
                connection_type: config.server.connection_type,
            },
            tls: TlsConfig {
                cert_path: config.tls.cert_path,
                key_path: config.tls.key_path,
                ca_cert_path: config.tls.ca_cert_path,
            },
            database: DatabaseConfig {
                dbname: if config.database.dbname.is_empty() { "public_keys.db".to_string() } else { config.database.dbname },
            },
            encryption: EncryptionConfig {
                aes_key: config.encryption.aes_key,
            },
        };

        tracing::info!("Configuration loaded successfully");
        Ok(config)
    }
}