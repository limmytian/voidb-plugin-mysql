//! MySQL target diagnostics for capability and profile-test paths.

use std::io::ErrorKind;

use mysql_async::{DriverError, Error as MySqlError, IoError, ServerError, UrlError};
use serde_json::{Value, json};
use voidb_core::{
    CapabilityError, CapabilityErrorCategory, RedactionStatus, TargetSystemFailure,
    redact_text_with_json,
};

use crate::config::{MySqlConfig, MySqlProfileViolation};

const PLUGIN_ID: &str = "mysql";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlDiagnosticStage {
    ProfileValidation,
    PoolCreate,
    Connect,
    Ping,
    Query,
    Metadata,
    DatabaseSelection,
}

impl MySqlDiagnosticStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProfileValidation => "profile_validation",
            Self::PoolCreate => "pool_create",
            Self::Connect => "connect",
            Self::Ping => "ping",
            Self::Query => "query",
            Self::Metadata => "metadata",
            Self::DatabaseSelection => "database_selection",
        }
    }
}

pub fn mysql_profile_error(
    config: &MySqlConfig,
    violations: Vec<MySqlProfileViolation>,
) -> CapabilityError {
    let fields = violations
        .iter()
        .map(|violation| violation.field)
        .collect::<Vec<_>>();
    let code = violations
        .first()
        .map(|violation| violation.code)
        .unwrap_or("validation.mysql_profile_invalid");
    let message = violations
        .first()
        .map(|violation| violation.message)
        .unwrap_or("MySQL profile is invalid.");

    capability_error(
        CapabilityErrorCategory::Validation,
        code,
        message,
        json!({
            "stage": MySqlDiagnosticStage::ProfileValidation.as_str(),
            "fields": fields,
            "profile": diagnostic_profile_summary(config),
        }),
        None,
        false,
        RedactionStatus::NotRequired,
    )
}

pub fn mysql_url_error(
    config: &MySqlConfig,
    stage: MySqlDiagnosticStage,
    error: &UrlError,
) -> CapabilityError {
    let (message, redaction) = redact_mysql_diagnostic(config, &error.to_string());
    capability_error(
        CapabilityErrorCategory::Validation,
        "validation.mysql_profile_url_invalid",
        "MySQL profile produced invalid connection options.",
        diagnostic_details(config, stage, Value::Null),
        Some(TargetSystemFailure {
            system: Some(PLUGIN_ID.to_string()),
            code: Some("mysql.url".to_string()),
            message: Some(message),
        }),
        false,
        redaction,
    )
}

pub fn mysql_pool_create_error(config: &MySqlConfig, message: &str) -> CapabilityError {
    let (message, redaction) = redact_mysql_diagnostic(config, message);
    capability_error(
        CapabilityErrorCategory::Validation,
        "validation.mysql_pool_options_invalid",
        "MySQL profile produced invalid pool options.",
        diagnostic_details(config, MySqlDiagnosticStage::PoolCreate, Value::Null),
        Some(TargetSystemFailure {
            system: Some(PLUGIN_ID.to_string()),
            code: Some("mysql.pool_options".to_string()),
            message: Some(message),
        }),
        false,
        redaction,
    )
}

pub fn mysql_error_to_capability_error(
    config: &MySqlConfig,
    stage: MySqlDiagnosticStage,
    error: &MySqlError,
) -> CapabilityError {
    match error {
        MySqlError::Server(server) => mysql_server_error(config, stage, server),
        MySqlError::Io(io) => mysql_io_error(config, stage, io),
        MySqlError::Driver(driver) => mysql_driver_error(config, stage, driver),
        MySqlError::Url(url) => mysql_url_error(config, stage, url),
        MySqlError::Other(_) => {
            let (message, redaction) = redact_mysql_diagnostic(config, &error.to_string());
            capability_error(
                CapabilityErrorCategory::Transport,
                "transport.mysql_other",
                "MySQL transport failed.",
                diagnostic_details(config, stage, Value::Null),
                Some(TargetSystemFailure {
                    system: Some(PLUGIN_ID.to_string()),
                    code: Some("mysql.other".to_string()),
                    message: Some(message),
                }),
                true,
                redaction,
            )
        }
    }
}

pub fn capability_error_to_legacy_message(error: &CapabilityError) -> String {
    let target_message = error
        .target
        .as_ref()
        .and_then(|target| target.message.as_deref())
        .unwrap_or(error.message.as_str());
    format!("{}: {}", error.code, target_message)
}

pub fn redact_mysql_diagnostic(
    config: &MySqlConfig,
    diagnostic: &str,
) -> (String, RedactionStatus) {
    let config_value = serde_json::to_value(config).unwrap_or_else(|_| json!({}));
    redact_text_with_json(diagnostic, &config_value)
}

fn mysql_server_error(
    config: &MySqlConfig,
    stage: MySqlDiagnosticStage,
    server: &ServerError,
) -> CapabilityError {
    let (category, code, message, retryable) = match server.code {
        1044 | 1045 | 1698 => (
            CapabilityErrorCategory::Auth,
            "auth.mysql_access_denied",
            "MySQL authentication failed.",
            false,
        ),
        1049 => (
            CapabilityErrorCategory::TargetSystem,
            "target.mysql_database_not_found",
            "MySQL selected database was not found.",
            false,
        ),
        1040 | 1203 => (
            CapabilityErrorCategory::Unavailable,
            "unavailable.mysql_connection_limit",
            "MySQL server is temporarily unavailable due to connection limits.",
            true,
        ),
        2006 | 2013 => (
            CapabilityErrorCategory::Transport,
            "transport.mysql_connection_lost",
            "MySQL connection was lost.",
            true,
        ),
        _ => (
            CapabilityErrorCategory::TargetSystem,
            "target.mysql_server_error",
            "MySQL target operation failed.",
            false,
        ),
    };
    let (target_message, redaction) = redact_mysql_diagnostic(config, &server.message);

    capability_error(
        category,
        code,
        message,
        diagnostic_details(
            config,
            stage,
            json!({
                "mysql_error_code": server.code,
                "sql_state": server.state,
            }),
        ),
        Some(TargetSystemFailure {
            system: Some(PLUGIN_ID.to_string()),
            code: Some(format!("{}:{}", server.state, server.code)),
            message: Some(target_message),
        }),
        retryable,
        redaction,
    )
}

fn mysql_io_error(
    config: &MySqlConfig,
    stage: MySqlDiagnosticStage,
    error: &IoError,
) -> CapabilityError {
    match error {
        IoError::Io(io) => mysql_std_io_error(config, stage, io),
        IoError::Tls(tls) => {
            let (message, redaction) = redact_mysql_diagnostic(config, &tls.to_string());
            capability_error(
                CapabilityErrorCategory::Transport,
                "transport.mysql_tls_failed",
                "MySQL TLS negotiation failed.",
                diagnostic_details(config, stage, json!({ "io_kind": "tls" })),
                Some(TargetSystemFailure {
                    system: Some(PLUGIN_ID.to_string()),
                    code: Some("mysql.tls".to_string()),
                    message: Some(message),
                }),
                false,
                redaction,
            )
        }
    }
}

fn mysql_std_io_error(
    config: &MySqlConfig,
    stage: MySqlDiagnosticStage,
    error: &std::io::Error,
) -> CapabilityError {
    let (category, code, message, retryable) = match error.kind() {
        ErrorKind::TimedOut => (
            CapabilityErrorCategory::Timeout,
            "timeout.mysql_target",
            "MySQL target timed out.",
            true,
        ),
        ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::NotConnected
        | ErrorKind::AddrNotAvailable
        | ErrorKind::AddrInUse
        | ErrorKind::BrokenPipe => (
            CapabilityErrorCategory::Transport,
            "transport.mysql_connection_failed",
            "MySQL network connection failed.",
            true,
        ),
        _ => (
            CapabilityErrorCategory::Transport,
            "transport.mysql_io_failed",
            "MySQL I/O failed.",
            true,
        ),
    };
    let (target_message, redaction) = redact_mysql_diagnostic(config, &error.to_string());

    capability_error(
        category,
        code,
        message,
        diagnostic_details(
            config,
            stage,
            json!({
                "io_kind": format!("{:?}", error.kind()),
            }),
        ),
        Some(TargetSystemFailure {
            system: Some(PLUGIN_ID.to_string()),
            code: Some(format!("io.{:?}", error.kind()).to_ascii_lowercase()),
            message: Some(target_message),
        }),
        retryable,
        redaction,
    )
}

fn mysql_driver_error(
    config: &MySqlConfig,
    stage: MySqlDiagnosticStage,
    error: &DriverError,
) -> CapabilityError {
    let (category, code, message, retryable) = match error {
        DriverError::UnknownAuthPlugin { .. }
        | DriverError::MysqlOldPasswordDisabled
        | DriverError::CleartextPluginDisabled => (
            CapabilityErrorCategory::Auth,
            "auth.mysql_auth_plugin_unsupported",
            "MySQL authentication plugin is unsupported.",
            false,
        ),
        DriverError::NoClientSslFlagFromServer => (
            CapabilityErrorCategory::Transport,
            "transport.mysql_tls_not_supported",
            "MySQL TLS was requested but the server does not support it.",
            false,
        ),
        DriverError::ConnectionClosed | DriverError::PoolDisconnected => (
            CapabilityErrorCategory::Transport,
            "transport.mysql_connection_closed",
            "MySQL connection is closed.",
            true,
        ),
        DriverError::PacketTooLarge => (
            CapabilityErrorCategory::TargetSystem,
            "target.mysql_packet_too_large",
            "MySQL packet was too large.",
            false,
        ),
        _ => (
            CapabilityErrorCategory::TargetSystem,
            "target.mysql_driver_error",
            "MySQL driver rejected the operation.",
            false,
        ),
    };
    let (target_message, redaction) = redact_mysql_diagnostic(config, &error.to_string());

    capability_error(
        category,
        code,
        message,
        diagnostic_details(
            config,
            stage,
            json!({
                "driver_error": format!("{:?}", error),
            }),
        ),
        Some(TargetSystemFailure {
            system: Some(PLUGIN_ID.to_string()),
            code: Some("mysql.driver".to_string()),
            message: Some(target_message),
        }),
        retryable,
        redaction,
    )
}

fn diagnostic_details(config: &MySqlConfig, stage: MySqlDiagnosticStage, extra: Value) -> Value {
    json!({
        "stage": stage.as_str(),
        "profile": diagnostic_profile_summary(config),
        "extra": extra,
    })
}

fn diagnostic_profile_summary(config: &MySqlConfig) -> Value {
    json!({
        "schema_id": crate::config::MYSQL_PROFILE_SCHEMA_ID,
        "endpoint": {
            "host_present": !config.host.trim().is_empty(),
            "port": config.port,
        },
        "database_selected": config.normalized_database().is_some(),
        "auth": {
            "username_present": !config.username.trim().is_empty(),
            "legacy_password_present": !config.password.is_empty(),
            "credential_ref_count": config.credential_refs.len(),
        },
        "tls": {
            "mode": config.normalized_ssl_mode(),
        },
        "charset": config.normalized_charset(),
        "pool_size": config.normalized_pool_size(),
        "connect_timeout_ms": config.connect_timeout_ms,
    })
}

fn capability_error(
    category: CapabilityErrorCategory,
    code: &str,
    message: &str,
    details: Value,
    target: Option<TargetSystemFailure>,
    retryable: bool,
    redaction: RedactionStatus,
) -> CapabilityError {
    CapabilityError {
        category,
        code: code.to_string(),
        message: message.to_string(),
        details,
        target,
        retryable,
        redaction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> MySqlConfig {
        MySqlConfig::new(
            "db.internal.example".into(),
            3306,
            "app".into(),
            "mysql-secret".into(),
        )
        .with_database("appdb".into())
    }

    #[test]
    fn maps_access_denied_to_auth_error() {
        let config = config();
        let error = MySqlError::Server(ServerError {
            code: 1045,
            state: "28000".into(),
            message: "Access denied for user app with password mysql-secret".into(),
        });

        let mapped =
            mysql_error_to_capability_error(&config, MySqlDiagnosticStage::Connect, &error);

        assert_eq!(mapped.category, CapabilityErrorCategory::Auth);
        assert_eq!(mapped.code, "auth.mysql_access_denied");
        assert_eq!(mapped.redaction, RedactionStatus::Applied);
        assert!(
            !mapped
                .target
                .as_ref()
                .and_then(|target| target.message.as_ref())
                .expect("target message")
                .contains("mysql-secret")
        );
    }

    #[test]
    fn maps_unknown_database_to_target_error() {
        let config = config();
        let error = MySqlError::Server(ServerError {
            code: 1049,
            state: "42000".into(),
            message: "Unknown database 'appdb'".into(),
        });

        let mapped = mysql_error_to_capability_error(
            &config,
            MySqlDiagnosticStage::DatabaseSelection,
            &error,
        );

        assert_eq!(mapped.category, CapabilityErrorCategory::TargetSystem);
        assert_eq!(mapped.code, "target.mysql_database_not_found");
    }

    #[test]
    fn maps_io_timeout_to_timeout_error() {
        let config = config();
        let error = MySqlError::Io(IoError::Io(std::io::Error::new(
            ErrorKind::TimedOut,
            "connection timed out",
        )));

        let mapped =
            mysql_error_to_capability_error(&config, MySqlDiagnosticStage::Connect, &error);

        assert_eq!(mapped.category, CapabilityErrorCategory::Timeout);
        assert_eq!(mapped.code, "timeout.mysql_target");
        assert!(mapped.retryable);
    }

    #[test]
    fn diagnostic_details_do_not_echo_profile_values() {
        let config = config();
        let error = MySqlError::Server(ServerError {
            code: 1049,
            state: "42000".into(),
            message: "Unknown database 'appdb'".into(),
        });

        let mapped = mysql_error_to_capability_error(
            &config,
            MySqlDiagnosticStage::DatabaseSelection,
            &error,
        );
        let details = serde_json::to_string(&mapped.details).unwrap();

        assert!(!details.contains("db.internal.example"));
        assert!(!details.contains("appdb"));
        assert!(!details.contains("mysql-secret"));
    }
}
