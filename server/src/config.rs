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

#[derive(Deserialize, Debug, Clone, Default)]
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
pub struct PerformanceConfig {
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval: u64,
    #[serde(default = "default_consumer_cleanup_interval")]
    pub consumer_cleanup_interval: u64,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_sec: u64,
}

fn default_max_queue_size() -> usize { 10_000 }
fn default_heartbeat_timeout() -> u64 { 60 }
fn default_cleanup_interval() -> u64 { 3000 }
fn default_consumer_cleanup_interval() -> u64 { 60 }
fn default_idle_timeout() -> u64 { 60 }

impl Default for PerformanceConfig {
    fn default() -> Self { Self { max_queue_size: default_max_queue_size(), heartbeat_timeout: default_heartbeat_timeout(), cleanup_interval: default_cleanup_interval(), consumer_cleanup_interval: default_consumer_cleanup_interval(), idle_timeout_sec: default_idle_timeout() } }
}

#[derive(Deserialize, Debug, Clone)]
pub struct AdminConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_admin_address")]
    pub address: String,
    #[serde(default)]
    pub allowed_cns: Vec<String>,
}
fn default_admin_address() -> String { "127.0.0.1:9091".to_string() }
impl Default for AdminConfig {
    fn default() -> Self { Self { enabled: false, address: default_admin_address(), allowed_cns: Vec::new() } }
}

#[derive(Deserialize, Debug, Clone)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_global_rps")]
    pub global_rps: f64,
    #[serde(default = "default_global_burst")]
    pub global_burst: f64,
    #[serde(default = "default_publish_rps")]
    pub publish_rps: f64,
    #[serde(default = "default_publish_burst")]
    pub publish_burst: f64,
    #[serde(default = "default_max_connections_per_ip")]
    pub max_connections_per_ip: usize,
    #[serde(default = "default_max_connections_per_cn")]
    pub max_connections_per_cn: usize,
}
fn default_global_rps() -> f64 { 100.0 }
fn default_global_burst() -> f64 { 200.0 }
fn default_publish_rps() -> f64 { 50.0 }
fn default_publish_burst() -> f64 { 100.0 }
fn default_max_connections_per_ip() -> usize { 50 }
fn default_max_connections_per_cn() -> usize { 20 }
impl Default for RateLimitConfig {
    fn default() -> Self { Self { enabled: false, global_rps: default_global_rps(), global_burst: default_global_burst(), publish_rps: default_publish_rps(), publish_burst: default_publish_burst(), max_connections_per_ip: default_max_connections_per_ip(), max_connections_per_cn: default_max_connections_per_cn() } }
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct AclConfig {
    #[serde(default)]
    pub sender_cns: Vec<String>,
    #[serde(default)]
    pub receiver_cns: Vec<String>,
    #[serde(default)]
    pub admin_cns: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    pub logging: LoggingConfig,
    pub database: DatabaseConfig,
    pub encryption: EncryptionConfig,
    #[serde(default)]
    pub performance: PerformanceConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub acl: AclConfig,
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
            performance: config.performance,
            admin: config.admin,
            rate_limit: config.rate_limit,
            acl: config.acl,
        };

        info!("Configuration loaded successfully");
        Ok(config)
    }
}