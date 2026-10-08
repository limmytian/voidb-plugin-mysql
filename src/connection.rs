/// MySQL Connection Manager integration.
///
/// Implements ConnectionDataProvider and ConnectionDisplayProvider traits
/// for MySQL plugin to provide connection metadata and display configuration.
use serde_json::{json, Value};
use std::collections::HashMap;

use voidb_core::connection::ConnectionConfig;
use voidb_core::plugin::{
    ConnectionDataProvider, ConnectionDisplayProvider, ConnectionSummary, DisplayLayout,
    ColumnDefinition, ColumnWidth, ColumnFormat, Alignment, Badge, BadgeStyle,
    FieldDefinition, FieldType, ValidationRule, QuickAction, DisplayConfig,
    ViewMode, DefaultColumn, SortOption, FilterOption,
};

/// MySQL plugin connection data provider
pub struct MySqlConnectionProvider;

impl MySqlConnectionProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MySqlConnectionProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionDataProvider for MySqlConnectionProvider {
    fn plugin_id(&self) -> &str {
        "mysql"
    }

    fn get_connection_summary(&self, config: &ConnectionConfig) -> ConnectionSummary {
        use crate::config::MySqlConfig;

        let mysql_config: MySqlConfig = config.plugin_config
            .as_ref()
            .and_then(|pc| serde_json::from_value(pc.clone()).ok())
            .unwrap_or_default();

        let host = &mysql_config.host;
        let port = mysql_config.port;
        let database = mysql_config.database.as_deref().unwrap_or("");
        let user = &mysql_config.username;

        // Determine if SSL is enabled
        let has_ssl = mysql_config
            .ssl_mode
            .as_ref()
            .map(|s| s == "required" || s == "preferred")
            .unwrap_or(false);

        // Build display layout
        let mut badges = Vec::new();
        if has_ssl {
            badges.push(Badge {
                text: "SSL".to_string(),
                style: BadgeStyle::Success,
                icon: Some("🔒".to_string()),
            });
        }

        // No custom columns - use default columns from DisplayConfig
        // Custom columns would be plugin-specific data not covered by standard fields
        let columns = vec![];

        // Build metadata
        let mut metadata = HashMap::new();
        if let Some(ref ssl_mode) = mysql_config.ssl_mode {
            metadata.insert("ssl_mode".to_string(), ssl_mode.clone());
        }

        ConnectionSummary {
            icon: "🗄️".to_string(),
            display_name: config.name.clone(),
            type_label: "MySQL".to_string(),
            primary_info: format!("{}:{}", host, port),
            secondary_info: if database.is_empty() {
                format!("user: {}", user)
            } else {
                format!("database: {} | user: {}", database, user)
            },
            tags: vec!["database".to_string(), "sql".to_string()],
            metadata,
            display_layout: DisplayLayout {
                columns,
                badges,
                inline_widgets: Vec::new(),
            },
        }
    }

    fn get_custom_fields(&self) -> Vec<FieldDefinition> {
        vec![
            FieldDefinition {
                name: "charset".to_string(),
                label: "Character Set".to_string(),
                field_type: FieldType::Select {
                    options: vec![
                        "utf8mb4".to_string(),
                        "utf8".to_string(),
                        "latin1".to_string(),
                        "ascii".to_string(),
                    ],
                },
                default_value: Some(json!("utf8mb4")),
                validation: ValidationRule::OneOf(vec![
                    "utf8mb4".to_string(),
                    "utf8".to_string(),
                    "latin1".to_string(),
                    "ascii".to_string(),
                ]),
                help_text: Some("Character encoding for the connection".to_string()),
                required: false,
            },
            FieldDefinition {
                name: "ssl_mode".to_string(),
                label: "SSL Mode".to_string(),
                field_type: FieldType::Select {
                    options: vec![
                        "disabled".to_string(),
                        "preferred".to_string(),
                        "required".to_string(),
                    ],
                },
                default_value: Some(json!("preferred")),
                validation: ValidationRule::OneOf(vec![
                    "disabled".to_string(),
                    "preferred".to_string(),
                    "required".to_string(),
                ]),
                help_text: Some("SSL/TLS connection mode".to_string()),
                required: false,
            },
            FieldDefinition {
                name: "pool_size".to_string(),
                label: "Connection Pool Size".to_string(),
                field_type: FieldType::Number,
                default_value: Some(json!(5)),
                validation: ValidationRule::Range { min: 1, max: 100 },
                help_text: Some("Maximum number of connections in the pool (1-100)".to_string()),
                required: false,
            },
        ]
    }

    fn validate_custom_field(&self, name: &str, value: &Value) -> Result<(), String> {
        match name {
            "charset" => {
                let charset = value
                    .as_str()
                    .ok_or_else(|| "Charset must be a string".to_string())?;
                let valid_charsets = ["utf8mb4", "utf8", "latin1", "ascii"];
                if !valid_charsets.contains(&charset) {
                    return Err(format!(
                        "Invalid charset '{}'. Must be one of: {}",
                        charset,
                        valid_charsets.join(", ")
                    ));
                }
                Ok(())
            }
            "ssl_mode" => {
                let ssl_mode = value
                    .as_str()
                    .ok_or_else(|| "SSL mode must be a string".to_string())?;
                let valid_modes = ["disabled", "preferred", "required"];
                if !valid_modes.contains(&ssl_mode) {
                    return Err(format!(
                        "Invalid SSL mode '{}'. Must be one of: {}",
                        ssl_mode,
                        valid_modes.join(", ")
                    ));
                }
                Ok(())
            }
            "pool_size" => {
                let pool_size = value
                    .as_i64()
                    .ok_or_else(|| "Pool size must be a number".to_string())?;
                if !(1..=100).contains(&pool_size) {
                    return Err(format!(
                        "Pool size must be between 1 and 100, got {}",
                        pool_size
                    ));
                }
                Ok(())
            }
            _ => Err(format!("Unknown field: {}", name)),
        }
    }

    fn get_quick_actions(&self) -> Vec<QuickAction> {
        vec![
            QuickAction {
                id: "open_sql_editor".to_string(),
                label: "SQL Editor".to_string(),
                icon: Some("📝".to_string()),
                description: Some("Open SQL query editor".to_string()),
            },
            QuickAction {
                id: "show_databases".to_string(),
                label: "Show Databases".to_string(),
                icon: Some("📂".to_string()),
                description: Some("List all databases".to_string()),
            },
            QuickAction {
                id: "connection_info".to_string(),
                label: "Info".to_string(),
                icon: Some("ℹ️".to_string()),
                description: Some("Show connection details".to_string()),
            },
        ]
    }
}

impl ConnectionDisplayProvider for MySqlConnectionProvider {
    fn plugin_id(&self) -> &str {
        "mysql"
    }

    fn get_display_config(&self) -> DisplayConfig {
        DisplayConfig {
            view_mode: ViewMode::Table,
            default_columns: vec![
                DefaultColumn::Status,
                DefaultColumn::Icon,
                DefaultColumn::Name,
                DefaultColumn::Type,
                DefaultColumn::Address,
                DefaultColumn::Database,
                DefaultColumn::User,
                DefaultColumn::LastUsed,
            ],
            custom_columns: vec![
                // SSL indicator column
                ColumnDefinition {
                    id: "ssl".to_string(),
                    label: "SSL".to_string(),
                    value: String::new(), // Will be populated dynamically
                    width: ColumnWidth::Fixed(5),
                    alignment: Alignment::Center,
                    format: ColumnFormat::Badge,
                },
            ],
            sort_options: vec![
                SortOption::Name,
                SortOption::LastUsed,
                SortOption::Status,
                SortOption::Custom("database".to_string()),
            ],
            filter_options: vec![
                FilterOption::ByStatus,
                FilterOption::ByTag,
                FilterOption::Custom {
                    id: "has_ssl".to_string(),
                    label: "With SSL".to_string(),
                    description: Some("Show only SSL-enabled connections".to_string()),
                },
            ],
        }
    }

    fn has_custom_rendering(&self) -> bool {
        false // Use default table rendering
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_connection_summary() {
        let provider = MySqlConnectionProvider::new();
        let mysql_config = json!({
            "host": "10.0.0.1",
            "port": 3306,
            "username": "root",
            "password": "password",
            "database": "testdb",
            "ssl_mode": "required"
        });
        let config = ConnectionConfig {
            name: "test-mysql".to_string(),
            db_type: voidb_core::connection::DatabaseType::MySQL,
            plugin_id: None,
            plugin_config: Some(mysql_config),
        };

        let summary = provider.get_connection_summary(&config);

        assert_eq!(summary.display_name, "test-mysql");
        assert_eq!(summary.type_label, "MySQL");
        assert_eq!(summary.primary_info, "10.0.0.1:3306");
        assert!(summary.secondary_info.contains("testdb"));
        assert!(summary.secondary_info.contains("root"));
        assert_eq!(summary.display_layout.badges.len(), 1);
        assert_eq!(summary.display_layout.badges[0].text, "SSL");
    }

    #[test]
    fn test_validate_custom_field() {
        let provider = MySqlConnectionProvider::new();

        // Valid charset
        assert!(provider
            .validate_custom_field("charset", &json!("utf8mb4"))
            .is_ok());

        // Invalid charset
        assert!(provider
            .validate_custom_field("charset", &json!("invalid"))
            .is_err());

        // Valid pool size
        assert!(provider
            .validate_custom_field("pool_size", &json!(5))
            .is_ok());

        // Invalid pool size (too large)
        assert!(provider
            .validate_custom_field("pool_size", &json!(150))
            .is_err());

        // Invalid pool size (too small)
        assert!(provider
            .validate_custom_field("pool_size", &json!(0))
            .is_err());
    }

    #[test]
    fn test_get_custom_fields() {
        let provider = MySqlConnectionProvider::new();
        let fields = provider.get_custom_fields();

        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].name, "charset");
        assert_eq!(fields[1].name, "ssl_mode");
        assert_eq!(fields[2].name, "pool_size");
    }

    #[test]
    fn test_get_quick_actions() {
        let provider = MySqlConnectionProvider::new();
        let actions = provider.get_quick_actions();

        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].id, "open_sql_editor");
        assert_eq!(actions[1].id, "show_databases");
        assert_eq!(actions[2].id, "connection_info");
    }
}
