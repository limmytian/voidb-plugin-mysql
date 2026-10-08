//! MySQL plugin configuration structures.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MYSQL_PROFILE_SCHEMA_ID: &str = "voidb.mysql.profile.v1";
pub const DEFAULT_MYSQL_PORT: u16 = 3306;
pub const DEFAULT_MYSQL_CHARSET: &str = "utf8mb4";
pub const DEFAULT_MYSQL_POOL_SIZE: u16 = 5;
pub const MAX_MYSQL_POOL_SIZE: u16 = 100;
pub const DEFAULT_MYSQL_SSL_MODE: &str = "preferred";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlCredentialRef {
    pub id: String,
    pub class: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// MySQL connection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySqlConfig {
    /// Database host
    pub host: String,
    /// Database port
    pub port: u16,
    /// Username
    pub username: String,
    /// Password
    pub password: String,
    /// Database name (optional, can be selected after connecting)
    pub database: Option<String>,
    /// SSL mode
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssl_mode: Option<String>,
    /// Preferred connection character set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    /// Maximum pool size requested by the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_size: Option<u16>,
    /// Connection timeout requested by capability and profile-test paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_ms: Option<u64>,
    /// Brokered credential references for capability-first profiles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credential_refs: Vec<MySqlCredentialRef>,
}

impl MySqlConfig {
    pub fn new(host: String, port: u16, username: String, password: String) -> Self {
        Self {
            host,
            port,
            username,
            password,
            database: None,
            ssl_mode: None,
            charset: None,
            pool_size: None,
            connect_timeout_ms: None,
            credential_refs: Vec::new(),
        }
    }

    pub fn with_database(mut self, database: String) -> Self {
        self.database = Some(database);
        self
    }

    pub fn with_ssl_mode(mut self, ssl_mode: String) -> Self {
        self.ssl_mode = Some(ssl_mode);
        self
    }

    pub fn normalized_ssl_mode(&self) -> &str {
        self.ssl_mode
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_MYSQL_SSL_MODE)
    }

    pub fn normalized_charset(&self) -> &str {
        self.charset
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_MYSQL_CHARSET)
    }

    pub fn normalized_pool_size(&self) -> u16 {
        self.pool_size.unwrap_or(DEFAULT_MYSQL_POOL_SIZE)
    }

    pub fn normalized_database(&self) -> Option<&str> {
        self.database
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Build connection URL for mysql_async
    pub fn to_url(&self) -> String {
        let db = self.normalized_database().unwrap_or("");
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.username, self.password, self.host, self.port, db
        )
    }

    /// Build a URL-shaped diagnostic that never includes credential material.
    pub fn to_redacted_url(&self) -> String {
        let db = self.normalized_database().unwrap_or("");
        format!(
            "mysql://{}:<redacted:password>@{}:{}/{}",
            self.username, self.host, self.port, db
        )
    }

    /// Return the stable, agent-facing profile schema for MySQL profiles.
    pub fn profile_schema() -> Value {
        json!({
            "$id": MYSQL_PROFILE_SCHEMA_ID,
            "type": "object",
            "required": ["host", "port", "username"],
            "properties": {
                "host": { "type": "string", "minLength": 1 },
                "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": DEFAULT_MYSQL_PORT },
                "username": { "type": "string", "minLength": 1 },
                "password": {
                    "type": "string",
                    "writeOnly": true,
                    "deprecated": true,
                    "description": "Legacy encrypted plugin_config password; capability profiles should use credential_refs."
                },
                "database": { "type": ["string", "null"], "minLength": 1 },
                "ssl_mode": {
                    "type": ["string", "null"],
                    "enum": ["disabled", "preferred", "required", "verify_ca", "verify_identity", null],
                    "default": DEFAULT_MYSQL_SSL_MODE
                },
                "charset": { "type": ["string", "null"], "default": DEFAULT_MYSQL_CHARSET },
                "pool_size": { "type": ["integer", "null"], "minimum": 1, "maximum": MAX_MYSQL_POOL_SIZE, "default": DEFAULT_MYSQL_POOL_SIZE },
                "connect_timeout_ms": { "type": ["integer", "null"], "minimum": 1 },
                "credential_refs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["id", "class"],
                        "properties": {
                            "id": { "type": "string", "minLength": 1 },
                            "class": { "type": "string", "enum": ["password", "token", "client_certificate"] },
                            "label": { "type": ["string", "null"] }
                        },
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        })
    }

    /// Return credential-free metadata for diagnostics and profile inspection.
    pub fn profile_metadata(&self) -> Value {
        json!({
            "schema_id": MYSQL_PROFILE_SCHEMA_ID,
            "endpoint": {
                "host": self.host,
                "port": self.port,
            },
            "database": self.normalized_database(),
            "auth": {
                "username": self.username,
                "legacy_password_present": !self.password.is_empty(),
                "credential_ref_count": self.credential_refs.len(),
                "credential_classes": self.credential_refs.iter().map(|item| item.class.as_str()).collect::<Vec<_>>(),
            },
            "tls": {
                "mode": self.normalized_ssl_mode(),
            },
            "charset": self.normalized_charset(),
            "pool_size": self.normalized_pool_size(),
            "connect_timeout_ms": self.connect_timeout_ms,
        })
    }

    pub fn validate_profile(&self) -> Vec<MySqlProfileViolation> {
        let mut violations = Vec::new();

        if self.host.trim().is_empty() {
            violations.push(MySqlProfileViolation::new(
                "host",
                "validation.mysql_profile_host_required",
                "MySQL host is required.",
            ));
        }
        if self.port == 0 {
            violations.push(MySqlProfileViolation::new(
                "port",
                "validation.mysql_profile_port_invalid",
                "MySQL port must be between 1 and 65535.",
            ));
        }
        if self.username.trim().is_empty() {
            violations.push(MySqlProfileViolation::new(
                "username",
                "validation.mysql_profile_username_required",
                "MySQL username is required.",
            ));
        }
        if let Some(database) = &self.database
            && database.trim().is_empty() {
                violations.push(MySqlProfileViolation::new(
                    "database",
                    "validation.mysql_profile_database_empty",
                    "MySQL database must be omitted instead of set to an empty string.",
                ));
            }
        let ssl_mode = self.normalized_ssl_mode();
        if !matches!(
            ssl_mode,
            "disabled" | "preferred" | "required" | "verify_ca" | "verify_identity"
        ) {
            violations.push(MySqlProfileViolation::new(
                "ssl_mode",
                "validation.mysql_profile_ssl_mode_invalid",
                "MySQL ssl_mode is not supported.",
            ));
        }
        if let Some(pool_size) = self.pool_size
            && !(1..=MAX_MYSQL_POOL_SIZE).contains(&pool_size) {
                violations.push(MySqlProfileViolation::new(
                    "pool_size",
                    "validation.mysql_profile_pool_size_invalid",
                    "MySQL pool_size must be between 1 and 100.",
                ));
            }
        for credential_ref in &self.credential_refs {
            if credential_ref.id.trim().is_empty() {
                violations.push(MySqlProfileViolation::new(
                    "credential_refs.id",
                    "validation.mysql_profile_credential_ref_id_required",
                    "MySQL credential reference id is required.",
                ));
            }
            if !matches!(
                credential_ref.class.as_str(),
                "password" | "token" | "client_certificate"
            ) {
                violations.push(MySqlProfileViolation::new(
                    "credential_refs.class",
                    "validation.mysql_profile_credential_ref_class_invalid",
                    "MySQL credential reference class is not supported.",
                ));
            }
        }

        violations
    }
}

impl Default for MySqlConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: DEFAULT_MYSQL_PORT,
            username: "root".to_string(),
            password: String::new(),
            database: None,
            ssl_mode: None,
            charset: None,
            pool_size: None,
            connect_timeout_ms: None,
            credential_refs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlProfileViolation {
    pub field: &'static str,
    pub code: &'static str,
    pub message: &'static str,
}

impl MySqlProfileViolation {
    fn new(field: &'static str, code: &'static str, message: &'static str) -> Self {
        Self {
            field,
            code,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_schema_documents_credentials_and_tls() {
        let schema = MySqlConfig::profile_schema();

        assert_eq!(schema["$id"], MYSQL_PROFILE_SCHEMA_ID);
        assert_eq!(schema["properties"]["password"]["writeOnly"], true);
        assert_eq!(schema["properties"]["password"]["deprecated"], true);
        assert!(
            schema["properties"]["ssl_mode"]["enum"]
                .as_array()
                .expect("ssl enum")
                .iter()
                .any(|value| value.as_str() == Some("verify_identity"))
        );
        assert_eq!(
            schema["properties"]["credential_refs"]["items"]["required"][0],
            "id"
        );
    }

    #[test]
    fn profile_metadata_never_contains_plaintext_password() {
        let config = MySqlConfig::new(
            "db.internal.example".into(),
            3306,
            "app".into(),
            "mysql-secret".into(),
        )
        .with_database("appdb".into());

        let metadata = config.profile_metadata();
        let encoded = serde_json::to_string(&metadata).unwrap();

        assert!(!encoded.contains("mysql-secret"));
        assert_eq!(metadata["auth"]["legacy_password_present"], true);
        assert_eq!(metadata["database"], "appdb");
    }

    #[test]
    fn validates_profile_shape() {
        let config = MySqlConfig {
            host: " ".into(),
            username: String::new(),
            pool_size: Some(MAX_MYSQL_POOL_SIZE + 1),
            ..MySqlConfig::default()
        };

        let violations = config.validate_profile();

        assert!(violations.iter().any(|item| item.field == "host"));
        assert!(violations.iter().any(|item| item.field == "username"));
        assert!(violations.iter().any(|item| item.field == "pool_size"));
    }
}
