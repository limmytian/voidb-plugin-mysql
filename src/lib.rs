mod agent_session;
mod capabilities;
mod cli_plugin;
mod config;
mod connection;
mod diagnostics;
pub mod service;

use std::time::Instant;

use async_trait::async_trait;
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Pool};

use voidb_core::connection::{ConnectionConfig, DatabaseType};
use voidb_core::database::DatabaseAdapter;
use voidb_core::database::types::*;
use voidb_core::error::VoidbError;
use voidb_core::plugin::NativePlugin;
use voidb_core::plugin::native::CreateAdapterFuture;

// Re-export config
pub use config::{
    MySqlConfig, MySqlCredentialRef, MySqlProfileViolation, MYSQL_PROFILE_SCHEMA_ID,
};
pub use capabilities::{invoke_mysql_capability, mysql_capabilities};
pub use agent_session::MySqlAgentSessionFactory;
pub use diagnostics::{
    MySqlDiagnosticStage, capability_error_to_legacy_message, mysql_error_to_capability_error,
    mysql_pool_create_error, mysql_profile_error, mysql_url_error, redact_mysql_diagnostic,
};

// Re-export CliPlugin factory
pub use cli_plugin::create_mysql_cli_plugin;

// Re-export Connection Manager providers
pub use connection::MySqlConnectionProvider;

// Re-export service layer for external consumers
pub use service::{MySqlService, MySqlCommand, MySqlEvent};

pub struct MySqlAdapter {
    pool: Pool,
    connection_id: String,
}

#[async_trait]
impl DatabaseAdapter for MySqlAdapter {
    async fn connect(config: &ConnectionConfig) -> Result<Box<dyn DatabaseAdapter>, VoidbError> {
        use crate::config::MySqlConfig;

        // Read configuration from plugin_config
        let mysql_config: MySqlConfig = config.plugin_config
            .as_ref()
            .ok_or_else(|| VoidbError::Connection("Missing plugin_config".to_string()))
            .and_then(|pc| {
                serde_json::from_value(pc.clone())
                    .map_err(|e| VoidbError::Connection(format!("Invalid MySQL config: {}", e)))
            })?;

        let opts = OptsBuilder::default()
            .ip_or_hostname(&mysql_config.host)
            .tcp_port(mysql_config.port)
            .user(Some(&mysql_config.username))
            .pass(Some(&mysql_config.password))
            .db_name(mysql_config.normalized_database());

        let pool = Pool::new(opts);

        // Test connection
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| {
                let error = crate::diagnostics::mysql_error_to_capability_error(
                    &mysql_config,
                    crate::diagnostics::MySqlDiagnosticStage::Connect,
                    &e,
                );
                VoidbError::Connection(crate::diagnostics::capability_error_to_legacy_message(
                    &error,
                ))
            })?;

        // Verify connection works
        conn.ping().await.map_err(|e| {
            let error = crate::diagnostics::mysql_error_to_capability_error(
                &mysql_config,
                crate::diagnostics::MySqlDiagnosticStage::Ping,
                &e,
            );
            VoidbError::Connection(crate::diagnostics::capability_error_to_legacy_message(
                &error,
            ))
        })?;

        drop(conn);

        Ok(Box::new(Self {
            pool,
            connection_id: config.name.clone(),
        }))
    }

    async fn ping(&self) -> Result<(), VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Connection(e.to_string()))?;

        conn.ping()
            .await
            .map_err(|e| VoidbError::Connection(e.to_string()))?;

        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), VoidbError> {
        // mysql_async Pool::disconnect consumes self, so we need to clone
        let pool = self.pool.clone();
        pool.disconnect()
            .await
            .map_err(|e| VoidbError::Connection(e.to_string()))?;
        Ok(())
    }

    fn db_type(&self) -> DatabaseType {
        DatabaseType::MySQL
    }

    fn connection_id(&self) -> String {
        self.connection_id.clone()
    }

    async fn list_databases(&self) -> Result<Vec<String>, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let databases: Vec<String> = conn
            .query("SHOW DATABASES")
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        Ok(databases)
    }

    async fn list_tables(&self, database: &str) -> Result<Vec<TableInfo>, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let sql = "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS
                   FROM INFORMATION_SCHEMA.TABLES
                   WHERE TABLE_SCHEMA = ?
                   ORDER BY TABLE_NAME";

        let tables: Vec<(String, String, Option<String>, Option<u64>)> = conn
            .exec(sql, (database,))
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        Ok(tables
            .into_iter()
            .map(|(name, table_type, comment, row_count)| TableInfo {
                name,
                table_type,
                comment,
                row_count,
            })
            .collect())
    }

    async fn list_views(&self, database: &str) -> Result<Vec<ViewInfo>, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let sql = "SELECT TABLE_NAME, VIEW_DEFINITION
                   FROM INFORMATION_SCHEMA.VIEWS
                   WHERE TABLE_SCHEMA = ?
                   ORDER BY TABLE_NAME";

        let views: Vec<(String, String)> = conn
            .exec(sql, (database,))
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        Ok(views
            .into_iter()
            .map(|(name, definition)| ViewInfo {
                name,
                definition: Some(definition),
            })
            .collect())
    }

    async fn describe_table(&self, database: &str, table: &str) -> Result<TableSchema, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let sql = "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY,
                          COLUMN_DEFAULT, CHARACTER_MAXIMUM_LENGTH, EXTRA
                   FROM INFORMATION_SCHEMA.COLUMNS
                   WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                   ORDER BY ORDINAL_POSITION";

        let rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<u64>,
            String,
        )> = conn
            .exec(sql, (database, table))
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let columns = rows
            .into_iter()
            .map(
                |(name, data_type, nullable, key, default_value, max_length, extra)| ColumnInfo {
                    name,
                    data_type,
                    nullable: nullable == "YES",
                    is_primary_key: key == "PRI",
                    default_value,
                    max_length,
                    extra,
                },
            )
            .collect();

        let indexes = self.list_indexes(database, table).await?;
        let foreign_keys = self.list_foreign_keys(database, table).await?;

        Ok(TableSchema {
            database: database.to_string(),
            table_name: table.to_string(),
            columns,
            indexes,
            foreign_keys,
            engine: None,
            comment: None,
        })
    }

    async fn list_indexes(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<IndexInfo>, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let sql = "SELECT INDEX_NAME, COLUMN_NAME, NON_UNIQUE, INDEX_TYPE
                   FROM INFORMATION_SCHEMA.STATISTICS
                   WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                   ORDER BY INDEX_NAME, SEQ_IN_INDEX";

        let rows: Vec<(String, String, i64, String)> = conn
            .exec(sql, (database, table))
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let mut indexes: std::collections::HashMap<String, IndexInfo> =
            std::collections::HashMap::new();

        for (index_name, column_name, non_unique, index_type) in rows {
            indexes
                .entry(index_name.clone())
                .or_insert_with(|| IndexInfo {
                    name: index_name,
                    columns: Vec::new(),
                    unique: non_unique == 0,
                    index_type,
                })
                .columns
                .push(column_name);
        }

        Ok(indexes.into_values().collect())
    }

    async fn list_foreign_keys(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let sql = "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_NAME,
                          k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE
                   FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE k
                   LEFT JOIN INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS r
                       ON k.CONSTRAINT_NAME = r.CONSTRAINT_NAME
                       AND k.CONSTRAINT_SCHEMA = r.CONSTRAINT_SCHEMA
                   WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ?
                     AND k.REFERENCED_TABLE_NAME IS NOT NULL
                   ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION";

        let rows: Vec<(
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        )> = conn
            .exec(sql, (database, table))
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let mut fks: std::collections::HashMap<String, ForeignKeyInfo> =
            std::collections::HashMap::new();

        for (fk_name, column_name, ref_table, ref_column, update_rule, delete_rule) in rows {
            let fk = fks
                .entry(fk_name.clone())
                .or_insert_with(|| ForeignKeyInfo {
                    name: fk_name,
                    columns: Vec::new(),
                    referenced_table: ref_table,
                    referenced_columns: Vec::new(),
                    on_update: update_rule.unwrap_or_else(|| "NO ACTION".to_string()),
                    on_delete: delete_rule.unwrap_or_else(|| "NO ACTION".to_string()),
                });
            fk.columns.push(column_name);
            fk.referenced_columns.push(ref_column);
        }

        Ok(fks.into_values().collect())
    }

    async fn query_rows(
        &self,
        sql: &str,
        _params: &[QueryParam],
        offset: u64,
        limit: u64,
    ) -> Result<QueryResult, VoidbError> {
        let paginated_sql = format!("{} LIMIT {} OFFSET {}", sql, limit, offset);
        // query_rows receives fully-qualified SQL (database.table), so no need to select database
        self.execute_query(&paginated_sql, None).await
    }

    async fn execute(&self, sql: &str, params: &[QueryParam]) -> Result<ExecuteResult, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let values: Vec<mysql_async::Value> = params.iter().map(query_param_to_value).collect();

        conn.exec_drop(sql, mysql_async::Params::Positional(values))
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let affected = conn.affected_rows();
        Ok(ExecuteResult {
            rows_affected: affected,
            last_insert_id: None,
        })
    }

    async fn execute_query(
        &self,
        sql: &str,
        database: Option<&str>,
    ) -> Result<QueryResult, VoidbError> {
        let start = Instant::now();
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        // Select database if specified (required for MySQL to execute queries without fully qualified table names)
        if let Some(db) = database
            && !db.is_empty()
        {
            let use_db_sql = format!("USE `{}`", db);
            conn.query_drop(&use_db_sql).await.map_err(|e| {
                VoidbError::Query(format!("Failed to select database '{}': {}", db, e))
            })?;
        }

        // Execute query and get result set
        let mut result = conn
            .query_iter(sql)
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        // Get column metadata - columns() returns Option<Arc<[Column]>>
        let columns_arc = result
            .columns()
            .clone()
            .unwrap_or_else(|| std::sync::Arc::new([]));
        let columns_meta: Vec<_> = columns_arc.iter().cloned().collect();

        // Convert column metadata
        let columns: Vec<ColumnInfo> = columns_meta
            .iter()
            .map(|col| ColumnInfo {
                name: col.name_str().to_string(),
                data_type: format!("{:?}", col.column_type()),
                nullable: true,
                is_primary_key: false,
                default_value: None,
                max_length: None,
                extra: String::new(),
            })
            .collect();

        // Collect all rows
        let result_set: Vec<mysql_async::Row> = result
            .collect()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let execution_time = start.elapsed();

        // Convert rows to our format
        let result_rows: Vec<Row> = result_set
            .iter()
            .map(|row| {
                let values = columns_meta
                    .iter()
                    .enumerate()
                    .map(|(i, _col)| {
                        // Use mysql_async's FromValue trait to decode values
                        Self::decode_value(row, i)
                    })
                    .collect();
                Row { values }
            })
            .collect();

        let row_count = result_rows.len() as u64;
        Ok(QueryResult {
            columns,
            rows: result_rows,
            execution_time,
            rows_affected: Some(row_count),
        })
    }

    /// Execute multiple statements on the same connection to preserve context
    async fn execute_statements(
        &self,
        statements: &[String],
        initial_database: Option<&str>,
    ) -> Result<Vec<QueryResult>, VoidbError> {
        // Get a single connection from the pool
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        // Select initial database if specified
        if let Some(db) = initial_database
            && !db.is_empty()
        {
            let use_db_sql = format!("USE `{}`", db);
            conn.query_drop(&use_db_sql).await.map_err(|e| {
                VoidbError::Query(format!("Failed to select database '{}': {}", db, e))
            })?;
        }

        let mut results = Vec::new();

        // Execute each statement on the same connection
        for sql in statements {
            let start = Instant::now();

            // Execute query and get result set
            let mut result = conn
                .query_iter(sql)
                .await
                .map_err(|e| VoidbError::Query(e.to_string()))?;

            // Get column metadata
            let columns_arc = result
                .columns()
                .clone()
                .unwrap_or_else(|| std::sync::Arc::new([]));
            let columns_meta: Vec<_> = columns_arc.iter().cloned().collect();

            // Convert column metadata
            let columns: Vec<ColumnInfo> = columns_meta
                .iter()
                .map(|col| ColumnInfo {
                    name: col.name_str().to_string(),
                    data_type: format!("{:?}", col.column_type()),
                    nullable: true,
                    is_primary_key: false,
                    default_value: None,
                    max_length: None,
                    extra: String::new(),
                })
                .collect();

            // Collect all rows
            let result_set: Vec<mysql_async::Row> = result
                .collect()
                .await
                .map_err(|e| VoidbError::Query(e.to_string()))?;

            let execution_time = start.elapsed();

            // Convert rows to our format
            let result_rows: Vec<Row> = result_set
                .iter()
                .map(|row| {
                    let values = columns_meta
                        .iter()
                        .enumerate()
                        .map(|(i, _col)| Self::decode_value(row, i))
                        .collect();
                    Row { values }
                })
                .collect();

            let row_count = result_rows.len() as u64;
            results.push(QueryResult {
                columns,
                rows: result_rows,
                execution_time,
                rows_affected: Some(row_count),
            });
        }

        Ok(results)
    }

    async fn count_rows(
        &self,
        database: &str,
        table: &str,
        filter: Option<&str>,
    ) -> Result<u64, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        let sql = if let Some(f) = filter {
            format!(
                "SELECT COUNT(*) FROM {}.{} WHERE {}",
                self.quote_identifier(database),
                self.quote_identifier(table),
                f
            )
        } else {
            format!(
                "SELECT COUNT(*) FROM {}.{}",
                self.quote_identifier(database),
                self.quote_identifier(table)
            )
        };

        let count: Option<u64> = conn
            .query_first(&sql)
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        Ok(count.unwrap_or(0))
    }

    async fn get_create_table_sql(
        &self,
        database: &str,
        table: &str,
    ) -> Result<String, VoidbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        // MySQL provides SHOW CREATE TABLE command
        let sql = format!("SHOW CREATE TABLE `{}`.`{}`", database, table);

        let result: Option<(String, String)> = conn
            .query_first(&sql)
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        match result {
            Some((_table_name, create_sql)) => Ok(create_sql),
            None => Err(VoidbError::Query(format!(
                "Table {}.{} not found",
                database, table
            ))),
        }
    }

    fn generate_create_table_sql(&self, schema: &TableSchema) -> String {
        let mut sql = format!(
            "CREATE TABLE {} (\n",
            self.quote_identifier(&schema.table_name)
        );

        let column_defs: Vec<String> = schema
            .columns
            .iter()
            .map(|col| {
                let mut def = format!("  {} {}", self.quote_identifier(&col.name), col.data_type);
                if !col.nullable {
                    def.push_str(" NOT NULL");
                }
                if let Some(ref default) = col.default_value {
                    def.push_str(&format!(" DEFAULT {}", default));
                }
                if !col.extra.is_empty() {
                    def.push_str(&format!(" {}", col.extra));
                }
                def
            })
            .collect();

        sql.push_str(&column_defs.join(",\n"));

        let pk_columns: Vec<&String> = schema
            .columns
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| &c.name)
            .collect();

        if !pk_columns.is_empty() {
            sql.push_str(",\n  PRIMARY KEY (");
            sql.push_str(
                &pk_columns
                    .iter()
                    .map(|c| self.quote_identifier(c))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            sql.push(')');
        }

        sql.push_str("\n)");
        sql
    }

    fn quote_identifier(&self, name: &str) -> String {
        format!("`{}`", name.replace('`', "``"))
    }
}

impl MySqlAdapter {
    /// Decode a value from a mysql_async Row at the given column index
    fn decode_value(row: &mysql_async::Row, idx: usize) -> CellValue {
        use mysql_async::Value;

        // Get the raw value using as_ref with the index
        let value = match row.as_ref(idx) {
            Some(v) => v,
            None => return CellValue::Null,
        };

        match value {
            Value::NULL => CellValue::Null,
            Value::Bytes(bytes) => {
                // Try to decode as UTF-8 string
                if let Ok(text) = String::from_utf8(bytes.clone()) {
                    CellValue::Text(text)
                } else {
                    CellValue::Blob(bytes.clone())
                }
            }
            Value::Int(i) => CellValue::Int(*i),
            Value::UInt(u) => CellValue::Int(*u as i64),
            Value::Float(f) => CellValue::Float(*f as f64),
            Value::Double(d) => CellValue::Float(*d),
            Value::Date(year, month, day, hour, min, sec, _micro) => {
                if *hour == 0 && *min == 0 && *sec == 0 {
                    // Date only
                    CellValue::Date(format!("{:04}-{:02}-{:02}", year, month, day))
                } else {
                    // DateTime
                    CellValue::DateTime(format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        year, month, day, hour, min, sec
                    ))
                }
            }
            Value::Time(neg, days, hours, minutes, seconds, _micros) => {
                let sign = if *neg { "-" } else { "" };
                if *days > 0 {
                    CellValue::Text(format!(
                        "{}{}d {:02}:{:02}:{:02}",
                        sign, days, hours, minutes, seconds
                    ))
                } else {
                    CellValue::Time(format!("{}{}:{:02}:{:02}", sign, hours, minutes, seconds))
                }
            }
        }
    }
}

/// Convert a QueryParam to a mysql_async::Value for prepared statements.
fn query_param_to_value(param: &QueryParam) -> mysql_async::Value {
    match param {
        QueryParam::Null => mysql_async::Value::NULL,
        QueryParam::Bool(b) => mysql_async::Value::Int(if *b { 1 } else { 0 }),
        QueryParam::Int(i) => mysql_async::Value::Int(*i),
        QueryParam::Float(f) => mysql_async::Value::Double(*f),
        QueryParam::Text(s) => mysql_async::Value::Bytes(s.as_bytes().to_vec()),
        QueryParam::Blob(b) => mysql_async::Value::Bytes(b.clone()),
    }
}

/// Native plugin descriptor for MySQL.
pub struct MySqlPlugin;

impl NativePlugin for MySqlPlugin {
    fn plugin_id(&self) -> &str {
        "mysql"
    }

    fn name(&self) -> &str {
        "MySQL"
    }

    fn protocols(&self) -> &[&str] {
        &["mysql", "mariadb"]
    }

    fn default_port(&self) -> u16 {
        3306
    }

    fn create_adapter(&self, config: &ConnectionConfig) -> CreateAdapterFuture {
        let config = config.clone();
        Box::pin(async move { MySqlAdapter::connect(&config).await })
    }
}
