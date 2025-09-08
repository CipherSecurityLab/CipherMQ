use serde::Deserialize;
use std::fs;
use toml;
use tracing::{error, info};
use thiserror::Error;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

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
pub struct LoggingConfig {
    pub level: String,
    pub rotation: String,
    pub info_file_path: String,
    pub debug_file_path: String,
    pub error_file_path: String,
    pub max_size_mb: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub dbname: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EncryptionConfig {
    pub algorithm: String,
    pub aes_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub tls: TlsConfig,
    pub logging: LoggingConfig,
    pub database: DatabaseConfig,
    pub encryption: EncryptionConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        info!("Loading configuration from {}", path);
        let config_content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&config_content)?;

        // Validate required fields
        if config.server.connection_type == "tls" {
            if config.tls.cert_path.is_none() {
                error!("Missing cert_path for TLS configuration");
                return Err(ConfigError::MissingField("tls.cert_path".to_string()));
            }
            if config.tls.key_path.is_none() {
                error!("Missing key_path for TLS configuration");
                return Err(ConfigError::MissingField("tls.key_path".to_string()));
            }
            if config.tls.ca_cert_path.is_none() {
                error!("Missing ca_cert_path for TLS configuration");
                return Err(ConfigError::MissingField("tls.ca_cert_path".to_string()));
            }
        }

        // Validate database configuration
        if config.database.host.is_empty() {
            error!("Missing host for database configuration");
            return Err(ConfigError::MissingField("database.host".to_string()));
        }
        if config.database.user.is_empty() {
            error!("Missing user for database configuration");
            return Err(ConfigError::MissingField("database.user".to_string()));
        }
        if config.database.dbname.is_empty() {
            error!("Missing dbname for database configuration");
            return Err(ConfigError::MissingField("database.dbname".to_string()));
        }

        // Validate encryption configuration
        if config.encryption.algorithm != "x25519_chacha20_poly1305" {
            error!("Unsupported encryption algorithm: {}", config.encryption.algorithm);
            return Err(ConfigError::MissingField("encryption.algorithm".to_string()));
        }
        if config.encryption.aes_key.is_empty() {
            error!("Missing AES key for encryption");
            return Err(ConfigError::MissingField("encryption.aes_key".to_string()));
        }
        let aes_key = BASE64.decode(&config.encryption.aes_key)
            .map_err(|e| ConfigError::MissingField(format!("Invalid AES key: {}", e)))?;
        if aes_key.len() != 32 {
            error!("AES key must be 32 bytes long");
            return Err(ConfigError::MissingField("encryption.aes_key".to_string()));
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
            logging: LoggingConfig {
                level: if config.logging.level.is_empty() { "info".to_string() } else { config.logging.level },
                rotation: if config.logging.rotation.is_empty() { "daily".to_string() } else { config.logging.rotation },
                info_file_path: if config.logging.info_file_path.is_empty() { "logs/info.log".to_string() } else { config.logging.info_file_path },
                debug_file_path: if config.logging.debug_file_path.is_empty() { "logs/debug.log".to_string() } else { config.logging.debug_file_path },
                error_file_path: if config.logging.error_file_path.is_empty() { "logs/error.log".to_string() } else { config.logging.error_file_path },
                max_size_mb: if config.logging.max_size_mb == 0 { 10 } else { config.logging.max_size_mb },
            },
            database: DatabaseConfig {
                host: config.database.host,
                port: config.database.port,
                user: config.database.user,
                password: config.database.password,
                dbname: config.database.dbname,
            },
            encryption: EncryptionConfig {
                algorithm: config.encryption.algorithm,
                aes_key: config.encryption.aes_key,
            },
        };

        info!("Configuration loaded successfully");
        Ok(config)
    }
}