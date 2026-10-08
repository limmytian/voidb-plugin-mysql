//! Browser inner service.
//!
//! Handles `BrowserCommand` variants: database/table DDL operations
//! (create, drop, rename, duplicate), table copy/paste operations,
//! schema editor support, and generic SQL execution.
//!
//! Copy/paste operations create temporary pools for source/target
//! connections within the async method scope. Progress events are
//! emitted during long-running operations so the TUI can update.

use std::sync::Arc;

use mysql_async::prelude::*;
use mysql_async::Pool;
use tokio::sync::mpsc;

use voidb_core::{
    CellValue, ColumnInfo, DatabaseClipboard, DatabaseDialect, Row, TabManager,
    TableClipboard, TableSchema, VoidbClipboard,
};

use super::commands::{BrowserCommand, TableCopyData};
use super::events::{BrowserEvent, MySqlEvent};
use super::value_convert::escape_value;

type ColumnMetadataRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<u64>,
    String,
);

/// Inner service for browser / DDL operations.
///
/// Holds the main connection pool for the current connection.
/// Copy/paste operations that connect to external sources create
/// their own temporary pools within the async methods.
///
/// Also holds a reference to the shared clipboard for copy operations
/// that need to write clipboard data.
pub struct BrowserService {
    pool: Pool,
    clipboard: Arc<tokio::sync::RwLock<Option<VoidbClipboard>>>,
}

impl BrowserService {
    /// Create a new browser service backed by the given pool.
    pub fn new(pool: Pool, clipboard: Arc<tokio::sync::RwLock<Option<VoidbClipboard>>>) -> Self {
        Self { pool, clipboard }
    }

    /// Dispatch a `BrowserCommand` to the appropriate handler method.
    ///
    /// Sends `BrowserEvent` (or `MySqlEvent::Error`) through the event
    /// channel and notifies the TUI via `render_tx.request_render()`.
    pub async fn handle(
        &self,
        cmd: BrowserCommand,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        render_tx: &Arc<dyn TabManager>,
    ) {
        match cmd {
            BrowserCommand::CreateDatabase { name } => {
                match self.create_database(&name).await {
                    Ok(()) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::DatabaseCreated { name },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::DropDatabase { name } => {
                match self.drop_database(&name).await {
                    Ok(()) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::DatabaseDropped { name },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::DropTable { database, table } => {
                match self.drop_table(&database, &table).await {
                    Ok(()) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::TableDropped { database, table },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::RenameTable {
                database,
                old_name,
                new_name,
            } => {
                match self.rename_table(&database, &old_name, &new_name).await {
                    Ok(()) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::TableRenamed {
                                database,
                                old_name,
                                new_name,
                            },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::DuplicateTable {
                database,
                source,
                target,
            } => {
                match self.duplicate_table(&database, &source, &target).await {
                    Ok(()) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::TableDuplicated {
                                database,
                                source,
                                target,
                            },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::CopyTables {
                source_url,
                connection_id,
                database,
                tables,
                with_data,
            } => {
                let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesStarted));
                let _ = render_tx.request_render();
                match self
                    .copy_tables_to_clipboard(
                        &source_url, &connection_id, &database, &tables, with_data,
                        event_tx, render_tx,
                    )
                    .await
                {
                    Ok(()) => {
                        let _ = event_tx
                            .send(MySqlEvent::Browser(BrowserEvent::CopyTablesDone));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::CopyDatabase {
                source_url,
                connection_id,
                database,
                with_data,
            } => {
                let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesStarted));
                let _ = render_tx.request_render();
                match self
                    .copy_database_to_clipboard(
                        &source_url, &connection_id, &database, with_data,
                        event_tx, render_tx,
                    )
                    .await
                {
                    Ok(()) => {
                        let _ = event_tx
                            .send(MySqlEvent::Browser(BrowserEvent::CopyTablesDone));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::PasteTables {
                target_database,
                table_schemas,
            } => {
                self.paste_tables(&target_database, &table_schemas, event_tx, render_tx)
                    .await;
            }

            BrowserCommand::ExecuteSql { sql } => {
                match self.execute_sql(&sql).await {
                    Ok(result) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::SqlExecuted { result },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::LoadColumnsForEditor { database, table } => {
                match self.load_columns_for_editor(&database, &table).await {
                    Ok(columns) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::ColumnsForEditorLoaded {
                                database,
                                table,
                                columns,
                            },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::ApplySchemaChanges {
                database,
                table,
                sql,
            } => {
                match self.apply_schema_changes(&sql).await {
                    Ok(()) => {
                        let _ = event_tx.send(MySqlEvent::Browser(
                            BrowserEvent::SchemaChangesApplied { database, table },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e));
                    }
                }
            }

            BrowserCommand::PasteDatabase {
                target_database,
                source_url,
                source_database,
                tables,
                with_data,
            } => {
                self.paste_database(
                    &target_database,
                    &source_url,
                    &source_database,
                    &tables,
                    with_data,
                    event_tx,
                    render_tx,
                )
                .await;
            }
        }
        let _ = render_tx.request_render();
    }

    // === DDL operations ===

    /// Create a new database.
    async fn create_database(&self, name: &str) -> Result<(), String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;
        let sql = format!("CREATE DATABASE `{}`", name.replace('`', "``"));
        conn.query_drop(&sql).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop an existing database.
    async fn drop_database(&self, name: &str) -> Result<(), String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;
        let sql = format!("DROP DATABASE `{}`", name.replace('`', "``"));
        conn.query_drop(&sql).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop a table from a database.
    async fn drop_table(&self, database: &str, table: &str) -> Result<(), String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;
        let sql = format!(
            "DROP TABLE `{}`.`{}`",
            database.replace('`', "``"),
            table.replace('`', "``")
        );
        conn.query_drop(&sql).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Rename a table within a database.
    async fn rename_table(
        &self,
        database: &str,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;
        let sql = format!(
            "RENAME TABLE `{}`.`{}` TO `{}`.`{}`",
            database.replace('`', "``"),
            old_name.replace('`', "``"),
            database.replace('`', "``"),
            new_name.replace('`', "``"),
        );
        conn.query_drop(&sql).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Duplicate a table (structure + data).
    async fn duplicate_table(
        &self,
        database: &str,
        source: &str,
        target: &str,
    ) -> Result<(), String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;
        let qdb = database.replace('`', "``");
        let qsrc = source.replace('`', "``");
        let qdst = target.replace('`', "``");

        let create_sql = format!("CREATE TABLE `{qdb}`.`{qdst}` LIKE `{qdb}`.`{qsrc}`");
        conn.query_drop(&create_sql)
            .await
            .map_err(|e| e.to_string())?;

        let insert_sql = format!(
            "INSERT INTO `{qdb}`.`{qdst}` SELECT * FROM `{qdb}`.`{qsrc}`"
        );
        conn.query_drop(&insert_sql)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    // === Copy/paste operations ===

    /// Copy tables to the shared clipboard.
    ///
    /// Connects to the source, fetches DDL + schema + optional data for
    /// each table, builds clipboard entries, and writes to the shared
    /// clipboard. Emits progress events for the TUI.
    #[allow(clippy::too_many_arguments)]
    async fn copy_tables_to_clipboard(
        &self,
        source_url: &str,
        connection_id: &str,
        database: &str,
        tables: &[String],
        with_data: bool,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        render_tx: &Arc<dyn TabManager>,
    ) -> Result<(), String> {
        let opts = mysql_async::Opts::from_url(source_url).map_err(|e| e.to_string())?;
        let source_pool = Pool::new(opts);
        let mut conn = source_pool
            .get_conn()
            .await
            .map_err(|e| e.to_string())?;

        let mut clips = Vec::new();
        for table in tables {
            // Get CREATE TABLE DDL
            let ddl_sql = format!(
                "SHOW CREATE TABLE `{}`.`{}`",
                database.replace('`', "``"),
                table.replace('`', "``")
            );
            let row: Option<(String, String)> = conn
                .query_first(&ddl_sql)
                .await
                .map_err(|e| e.to_string())?;
            let create_sql = row.map(|(_, ddl)| ddl).unwrap_or_default();

            // Describe table schema
            let schema = describe_table(&mut conn, database, table).await?;

            // Optionally read data
            let data = if with_data {
                let select_sql = format!(
                    "SELECT * FROM `{}`.`{}`",
                    database.replace('`', "``"),
                    table.replace('`', "``")
                );
                let result: Vec<mysql_async::Row> = conn
                    .query(&select_sql)
                    .await
                    .map_err(|e| e.to_string())?;
                let rows: Vec<Row> = result
                    .iter()
                    .map(|r| {
                        let values: Vec<CellValue> =
                            (0..r.columns().len()).map(|i| mysql_row_to_cell(r, i)).collect();
                        Row { values }
                    })
                    .collect();
                Some(rows)
            } else {
                None
            };

            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesProgress {
                table: table.clone(),
                rows: data.as_ref().map_or(0, |d| d.len()),
            }));
            let _ = render_tx.request_render();

            clips.push(TableClipboard {
                dialect: DatabaseDialect::MySQL,
                source_connection: connection_id.to_string(),
                source_database: database.to_string(),
                table_name: table.clone(),
                create_table_sql: create_sql,
                schema,
                data,
            });
        }

        // Write to shared clipboard
        let clip = if clips.len() == 1 {
            VoidbClipboard::Table(Box::new(clips.into_iter().next().unwrap()))
        } else {
            VoidbClipboard::Tables(clips)
        };
        if let Ok(mut guard) = self.clipboard.try_write() {
            *guard = Some(clip);
        }

        let _ = source_pool.disconnect().await;
        Ok(())
    }

    /// Copy an entire database to the shared clipboard.
    ///
    /// Lists all tables in the database, fetches DDL + schema + optional
    /// data for each, and writes a `DatabaseClipboard` to the shared
    /// clipboard. Emits progress events for the TUI.
    async fn copy_database_to_clipboard(
        &self,
        source_url: &str,
        connection_id: &str,
        database: &str,
        with_data: bool,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        render_tx: &Arc<dyn TabManager>,
    ) -> Result<(), String> {
        let opts = mysql_async::Opts::from_url(source_url).map_err(|e| e.to_string())?;
        let source_pool = Pool::new(opts);
        let mut conn = source_pool
            .get_conn()
            .await
            .map_err(|e| e.to_string())?;

        // List all base tables in the database
        let tables_sql = format!(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_SCHEMA = '{}' AND TABLE_TYPE = 'BASE TABLE'",
            database.replace('\'', "''")
        );
        let table_names: Vec<String> = conn
            .query(&tables_sql)
            .await
            .map_err(|e| e.to_string())?;

        let mut clips = Vec::new();
        for table in &table_names {
            let ddl_sql = format!(
                "SHOW CREATE TABLE `{}`.`{}`",
                database.replace('`', "``"),
                table.replace('`', "``")
            );
            let row: Option<(String, String)> = conn
                .query_first(&ddl_sql)
                .await
                .map_err(|e| e.to_string())?;
            let create_sql = row.map(|(_, ddl)| ddl).unwrap_or_default();

            let schema = describe_table(&mut conn, database, table).await?;

            let data = if with_data {
                let select_sql = format!(
                    "SELECT * FROM `{}`.`{}`",
                    database.replace('`', "``"),
                    table.replace('`', "``")
                );
                let result: Vec<mysql_async::Row> = conn
                    .query(&select_sql)
                    .await
                    .map_err(|e| e.to_string())?;
                let rows: Vec<Row> = result
                    .iter()
                    .map(|r| {
                        let values: Vec<CellValue> =
                            (0..r.columns().len()).map(|i| mysql_row_to_cell(r, i)).collect();
                        Row { values }
                    })
                    .collect();
                Some(rows)
            } else {
                None
            };

            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesProgress {
                table: table.clone(),
                rows: data.as_ref().map_or(0, |d| d.len()),
            }));
            let _ = render_tx.request_render();

            clips.push(TableClipboard {
                dialect: DatabaseDialect::MySQL,
                source_connection: connection_id.to_string(),
                source_database: database.to_string(),
                table_name: table.clone(),
                create_table_sql: create_sql,
                schema,
                data,
            });
        }

        // Write database clipboard
        let db_clip = DatabaseClipboard {
            dialect: DatabaseDialect::MySQL,
            source_connection: connection_id.to_string(),
            database_name: database.to_string(),
            tables: clips,
        };
        if let Ok(mut guard) = self.clipboard.try_write() {
            *guard = Some(VoidbClipboard::Database(db_clip));
        }

        let _ = source_pool.disconnect().await;
        Ok(())
    }

    /// Paste tables from clipboard data into a target database.
    ///
    /// For each table: creates the table from DDL, then optionally copies
    /// data from the source connection. Emits progress events.
    async fn paste_tables(
        &self,
        target_database: &str,
        table_schemas: &[TableCopyData],
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        render_tx: &Arc<dyn TabManager>,
    ) {
        let mut conn = match self.pool.get_conn().await {
            Ok(c) => c,
            Err(e) => {
                let _ = event_tx.send(MySqlEvent::Error(format!(
                    "Connection failed: {}",
                    e
                )));
                return;
            }
        };

        for schema in table_schemas {
            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesProgress {
                table: schema.table_name.clone(),
                rows: 0,
            }));
            let _ = render_tx.request_render();

            // Replace database references in DDL
            let ddl = schema.create_sql.clone();

            // Execute CREATE TABLE
            if let Err(e) = conn.query_drop(&ddl).await {
                let _ = event_tx.send(MySqlEvent::Error(format!(
                    "CREATE TABLE '{}' failed: {}",
                    schema.table_name, e
                )));
                continue;
            }

            // Copy data if requested
            let mut rows_copied = 0usize;
            if schema.with_data {
                match self
                    .copy_rows_from_source(
                        &mut conn,
                        &schema.source_url,
                        target_database,
                        &schema.table_name,
                    )
                    .await
                {
                    Ok(count) => rows_copied = count,
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(format!(
                            "Data copy for '{}' failed: {}",
                            schema.table_name, e
                        )));
                    }
                }
            }

            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesProgress {
                table: schema.table_name.clone(),
                rows: rows_copied,
            }));
            let _ = render_tx.request_render();
        }

        let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::CopyTablesDone));
    }

    /// Paste a full database: create the database, then paste all tables.
    ///
    /// Creates a temporary pool for the source URL and copies each table's
    /// DDL and data from the source into the new target database.
    #[allow(clippy::too_many_arguments)]
    async fn paste_database(
        &self,
        target_database: &str,
        source_url: &str,
        source_database: &str,
        tables: &[String],
        with_data: bool,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        render_tx: &Arc<dyn TabManager>,
    ) {
        let mut conn = match self.pool.get_conn().await {
            Ok(c) => c,
            Err(e) => {
                let _ = event_tx.send(MySqlEvent::Error(format!(
                    "Connection failed: {}",
                    e
                )));
                return;
            }
        };

        // Create the target database
        let create_db = format!(
            "CREATE DATABASE `{}`",
            target_database.replace('`', "``")
        );
        if let Err(e) = conn.query_drop(&create_db).await {
            let _ = event_tx.send(MySqlEvent::Error(format!(
                "CREATE DATABASE failed: {}",
                e
            )));
            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::PasteDatabaseDone));
            return;
        }

        // Connect to source to copy DDL and data
        let source_opts = match mysql_async::Opts::from_url(source_url) {
            Ok(o) => o,
            Err(e) => {
                let _ = event_tx.send(MySqlEvent::Error(format!(
                    "Invalid source URL: {}",
                    e
                )));
                let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::PasteDatabaseDone));
                return;
            }
        };
        let source_pool = Pool::new(source_opts);
        let mut source_conn = match source_pool.get_conn().await {
            Ok(c) => c,
            Err(e) => {
                let _ = event_tx.send(MySqlEvent::Error(format!(
                    "Source connection failed: {}",
                    e
                )));
                let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::PasteDatabaseDone));
                return;
            }
        };

        for table in tables {
            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::PasteDatabaseProgress {
                table: table.clone(),
                rows: 0,
            }));
            let _ = render_tx.request_render();

            // Get DDL from source
            let ddl_sql = format!(
                "SHOW CREATE TABLE `{}`.`{}`",
                source_database.replace('`', "``"),
                table.replace('`', "``")
            );
            let row: Option<(String, String)> =
                match source_conn.query_first(&ddl_sql).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(format!(
                            "Failed to get DDL for '{}': {}",
                            table, e
                        )));
                        continue;
                    }
                };
            let create_sql = row.map(|(_, ddl)| ddl).unwrap_or_default();

            // Rewrite DDL to target database
            let ddl = create_sql.replace(
                &format!("`{}`.", source_database.replace('`', "``")),
                &format!("`{}`.", target_database.replace('`', "``")),
            );
            let ddl = if !ddl.contains(&format!("`{}`.", target_database)) {
                format!(
                    "USE `{}`; {}",
                    target_database.replace('`', "``"),
                    ddl
                )
            } else {
                ddl
            };

            // Execute DDL on target
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                if let Err(e) = conn.query_drop(stmt).await {
                    let _ = event_tx.send(MySqlEvent::Error(format!(
                        "DDL for '{}' failed: {}",
                        table, e
                    )));
                }
            }

            // Copy data if requested
            let mut rows_inserted = 0usize;
            if with_data {
                match self
                    .copy_data_between_dbs(
                        &mut source_conn,
                        &mut conn,
                        source_database,
                        target_database,
                        table,
                    )
                    .await
                {
                    Ok(count) => rows_inserted = count,
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(format!(
                            "Data copy for '{}' failed: {}",
                            table, e
                        )));
                    }
                }
            }

            let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::PasteDatabaseProgress {
                table: table.clone(),
                rows: rows_inserted,
            }));
            let _ = render_tx.request_render();
        }

        let _ = source_pool.disconnect().await;
        let _ = event_tx.send(MySqlEvent::Browser(BrowserEvent::PasteDatabaseDone));
    }

    // === SQL execution ===

    /// Execute raw SQL statement(s) in browser context.
    ///
    /// Splits on `;` and executes each statement sequentially.
    /// Returns a summary string like "OK (3 statements)".
    async fn execute_sql(&self, sql: &str) -> Result<String, String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;

        let stmts: Vec<&str> = sql
            .split(';')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        for stmt in &stmts {
            conn.query_drop(*stmt)
                .await
                .map_err(|e| format!("Failed: {}", e))?;
        }

        Ok(format!("OK ({} statements)", stmts.len()))
    }

    // === Schema editor support ===

    /// Load full column metadata for the schema editor.
    ///
    /// Uses INFORMATION_SCHEMA to get complete column details including
    /// types, nullability, keys, defaults, and extras.
    async fn load_columns_for_editor(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;

        let col_sql = format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, \
             COLUMN_DEFAULT, CHARACTER_MAXIMUM_LENGTH, EXTRA \
             FROM INFORMATION_SCHEMA.COLUMNS \
             WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' \
             ORDER BY ORDINAL_POSITION",
            database.replace('\'', "''"),
            table.replace('\'', "''")
        );

        let col_rows: Vec<ColumnMetadataRow> =
            conn.query(&col_sql).await.map_err(|e| e.to_string())?;

        let columns: Vec<ColumnInfo> = col_rows
            .iter()
            .map(|(name, dt, nullable, key, def, ml, extra)| ColumnInfo {
                name: name.clone(),
                data_type: dt.clone(),
                nullable: nullable == "YES",
                is_primary_key: key == "PRI",
                default_value: def.clone(),
                max_length: *ml,
                extra: extra.clone(),
            })
            .collect();

        Ok(columns)
    }

    /// Apply schema changes from the schema editor.
    ///
    /// Executes ALTER TABLE statements generated by the schema editor.
    async fn apply_schema_changes(&self, statements: &[String]) -> Result<(), String> {
        let mut conn = self.pool.get_conn().await.map_err(|e| e.to_string())?;

        for stmt in statements {
            if stmt.starts_with("--") {
                continue;
            }
            conn.query_drop(stmt)
                .await
                .map_err(|e| format!("Schema change failed: {}", e))?;
        }

        Ok(())
    }

    // === Private helpers ===

    /// Copy rows from a source connection to a table on the target (self.pool).
    ///
    /// Opens a temporary pool to the source URL, reads all rows, and
    /// inserts them in chunks of 500 into the target database.
    async fn copy_rows_from_source(
        &self,
        target_conn: &mut mysql_async::Conn,
        source_url: &str,
        target_database: &str,
        table_name: &str,
    ) -> Result<usize, String> {
        let source_opts = mysql_async::Opts::from_url(source_url).map_err(|e| e.to_string())?;
        let source_pool = Pool::new(source_opts);
        let mut source_conn = source_pool
            .get_conn()
            .await
            .map_err(|e| e.to_string())?;

        // Describe the table to get column names
        let schema = describe_table(&mut source_conn, target_database, table_name).await?;
        let col_names: Vec<String> = schema
            .columns
            .iter()
            .map(|c| format!("`{}`", c.name.replace('`', "``")))
            .collect();
        let cols_str = col_names.join(", ");

        // Select all data from source
        let select_sql = format!(
            "SELECT * FROM `{}`.`{}`",
            target_database.replace('`', "``"),
            table_name.replace('`', "``")
        );
        let result: Vec<mysql_async::Row> = source_conn
            .query(&select_sql)
            .await
            .map_err(|e| e.to_string())?;

        let rows: Vec<Row> = result
            .iter()
            .map(|r| {
                let values: Vec<CellValue> =
                    (0..r.columns().len()).map(|i| mysql_row_to_cell(r, i)).collect();
                Row { values }
            })
            .collect();

        let mut total_inserted = 0usize;
        for chunk in rows.chunks(500) {
            let mut values_parts = Vec::new();
            for row in chunk {
                let vals: Vec<String> = row.values.iter().map(mysql_quote_cell).collect();
                values_parts.push(format!("({})", vals.join(", ")));
            }
            let insert_sql = format!(
                "INSERT INTO `{}`.`{}` ({}) VALUES {}",
                target_database.replace('`', "``"),
                table_name.replace('`', "``"),
                cols_str,
                values_parts.join(", ")
            );
            target_conn
                .query_drop(&insert_sql)
                .await
                .map_err(|e| format!("INSERT into '{}' failed: {}", table_name, e))?;
            total_inserted += chunk.len();
        }

        let _ = source_pool.disconnect().await;
        Ok(total_inserted)
    }

    /// Copy data between two open connections (source -> target) for a specific table.
    async fn copy_data_between_dbs(
        &self,
        source_conn: &mut mysql_async::Conn,
        target_conn: &mut mysql_async::Conn,
        source_database: &str,
        target_database: &str,
        table: &str,
    ) -> Result<usize, String> {
        // Get column names from source
        let schema = describe_table(source_conn, source_database, table).await?;
        let col_names: Vec<String> = schema
            .columns
            .iter()
            .map(|c| format!("`{}`", c.name.replace('`', "``")))
            .collect();
        let cols_str = col_names.join(", ");

        // Select all data from source
        let select_sql = format!(
            "SELECT * FROM `{}`.`{}`",
            source_database.replace('`', "``"),
            table.replace('`', "``")
        );
        let result: Vec<mysql_async::Row> = source_conn
            .query(&select_sql)
            .await
            .map_err(|e| e.to_string())?;

        let rows: Vec<Row> = result
            .iter()
            .map(|r| {
                let values: Vec<CellValue> =
                    (0..r.columns().len()).map(|i| mysql_row_to_cell(r, i)).collect();
                Row { values }
            })
            .collect();

        let mut total_inserted = 0usize;
        for chunk in rows.chunks(500) {
            let mut values_parts = Vec::new();
            for row in chunk {
                let vals: Vec<String> = row.values.iter().map(mysql_quote_cell).collect();
                values_parts.push(format!("({})", vals.join(", ")));
            }
            let insert_sql = format!(
                "INSERT INTO `{}`.`{}` ({}) VALUES {}",
                target_database.replace('`', "``"),
                table.replace('`', "``"),
                cols_str,
                values_parts.join(", ")
            );
            target_conn
                .query_drop(&insert_sql)
                .await
                .map_err(|e| format!("INSERT into '{}' failed: {}", table, e))?;
            total_inserted += chunk.len();
        }

        Ok(total_inserted)
    }
}

// ---------------------------------------------------------------------------
// Private helpers (extracted from db_browser.rs free functions)
// ---------------------------------------------------------------------------

/// Describe a table using INFORMATION_SCHEMA to get column metadata.
async fn describe_table(
    conn: &mut mysql_async::Conn,
    db: &str,
    table: &str,
) -> Result<TableSchema, String> {
    let col_sql = format!(
        "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, \
         COLUMN_DEFAULT, CHARACTER_MAXIMUM_LENGTH, EXTRA \
         FROM INFORMATION_SCHEMA.COLUMNS \
         WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' \
         ORDER BY ORDINAL_POSITION",
        db.replace('\'', "''"),
        table.replace('\'', "''")
    );

    let col_rows: Vec<ColumnMetadataRow> = conn.query(&col_sql).await.map_err(|e| e.to_string())?;

    let columns: Vec<ColumnInfo> = col_rows
        .iter()
        .map(|(name, dt, nullable, key, def, ml, extra)| ColumnInfo {
            name: name.clone(),
            data_type: dt.clone(),
            nullable: nullable == "YES",
            is_primary_key: key == "PRI",
            default_value: def.clone(),
            max_length: *ml,
            extra: extra.clone(),
        })
        .collect();

    Ok(TableSchema {
        database: db.to_string(),
        table_name: table.to_string(),
        columns,
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        engine: None,
        comment: None,
    })
}

/// Convert a mysql_async Row value at a given index to CellValue.
fn mysql_row_to_cell(row: &mysql_async::Row, idx: usize) -> CellValue {
    use mysql_async::Value;
    match row.as_ref(idx) {
        Some(Value::NULL) | None => CellValue::Null,
        Some(Value::Int(i)) => CellValue::Int(*i),
        Some(Value::UInt(u)) => CellValue::Int(*u as i64),
        Some(Value::Float(f)) => CellValue::Float(*f as f64),
        Some(Value::Double(d)) => CellValue::Float(*d),
        Some(Value::Bytes(b)) => match String::from_utf8(b.clone()) {
            Ok(s) => CellValue::Text(s),
            Err(_) => CellValue::Blob(b.clone()),
        },
        Some(Value::Date(y, m, d, h, mi, s, _us)) => {
            CellValue::DateTime(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                y, m, d, h, mi, s
            ))
        }
        Some(Value::Time(neg, _d, h, m, s, _us)) => {
            let sign = if *neg { "-" } else { "" };
            CellValue::Time(format!("{}{:02}:{:02}:{:02}", sign, h, m, s))
        }
    }
}

/// Quote a CellValue for use in SQL INSERT statements.
fn mysql_quote_cell(cell: &CellValue) -> String {
    // Reuse escape_value from value_convert for most types;
    // this handles the full CellValue enum.
    escape_value(cell)
}
