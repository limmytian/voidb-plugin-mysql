//! MySQL service layer.
//!
//! This module provides `MySqlService`, the service facade for the MySQL
//! plugin. It follows the service-layer convention established in Phase 1:
//!
//! - Background tokio task processes commands asynchronously
//! - `send()` dispatches commands via unbounded mpsc channel (non-blocking)
//! - `poll_event()` drains events via `try_recv()` (non-blocking)
//! - Render notifications fire after every event emission (per D-08/D-11)
//!
//! # Inner Services
//!
//! The background task delegates to domain-specific inner services:
//! - `SchemaService` -- database/table/column/index introspection
//! - `CrudService` -- query execution, save changes, insert rows
//! - `DataLoaderService` -- paginated data loading, row counts, PKs
//! - `BrowserService` -- DDL management (stub for now, fleshed out in Plan 03)
//!
//! # Connection Lifecycle
//!
//! The service starts without a pool. A `Connect` command creates the pool
//! and instantiates inner services. On `Disconnect` (or sender drop), the
//! pool is cleanly disconnected via `pool.disconnect().await`.

pub mod browser;
pub mod commands;
pub mod connection;
pub mod crud;
pub mod data_loader;
pub mod events;
pub mod schema;
pub mod value_convert;

// Re-export all public types for ergonomic imports
pub use commands::{
    BrowserCommand, CrudCommand, LoadCommand, MySqlCommand, SchemaCommand, TableCopyData,
};
pub use events::{
    BrowserEvent, CrudEvent, IndexInfo, LoadEvent, MySqlEvent, SchemaEvent, StatementResult,
    TableMeta,
};
pub use voidb_core::database::types::ForeignKeyInfo;

use std::sync::Arc;

use mysql_async::prelude::*;
use tokio::sync::mpsc;
use voidb_core::database::types::ColumnInfo;

use crate::config::MySqlConfig;
use crate::diagnostics::{
    MySqlDiagnosticStage, capability_error_to_legacy_message, mysql_error_to_capability_error,
    mysql_pool_create_error, mysql_profile_error,
};
use connection::create_pool;
use crud::CrudService;
use data_loader::DataLoaderService;
use schema::SchemaService;
use voidb_core::{CapabilityError, TabManager, VoidbClipboard};

use browser::BrowserService;

/// Internal mode discriminator for MySqlService.
///
/// `Channel` mode is used by the TUI: commands are sent asynchronously
/// and results are polled. `Direct` mode is used by the CLI: async methods
/// return results directly.
enum ServiceMode {
    Channel {
        cmd_tx: mpsc::UnboundedSender<MySqlCommand>,
        event_rx: mpsc::UnboundedReceiver<MySqlEvent>,
        _task: tokio::task::JoinHandle<()>,
    },
    Direct {
        schema_svc: SchemaService,
        crud_svc: CrudService,
        _loader_svc: DataLoaderService,
    },
}

/// MySQL service facade.
///
/// Owns either channel-based TUI communication or direct async service
/// instances depending on the mode.
///
/// # Send + Sync
///
/// `MySqlService` is `Send` but NOT `Sync` (because `UnboundedReceiver`
/// is `!Sync`). Plugin structs must wrap it in `std::sync::Mutex` to
/// satisfy `Plugin: Send + Sync`. Since `Plugin::update(&mut self)` has
/// exclusive access, the Mutex is never contended.
pub struct MySqlService {
    mode: ServiceMode,
}

/// One authenticated MySQL connection retained for an agent session.
pub struct PersistentMySqlConnection {
    conn: Option<mysql_async::Conn>,
    pool: Option<mysql_async::Pool>,
    database: String,
}

impl PersistentMySqlConnection {
    pub async fn open(config: &MySqlConfig) -> Result<Self, String> {
        let violations = config.validate_profile();
        if !violations.is_empty() {
            return Err("Invalid MySQL session profile".into());
        }
        let pool = create_pool(config)?;
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|_| "MySQL session connection failed".to_string())?;
        conn.ping()
            .await
            .map_err(|_| "MySQL session ping failed".to_string())?;
        Ok(Self {
            conn: Some(conn),
            pool: Some(pool),
            database: config.normalized_database().unwrap_or("").to_string(),
        })
    }

    pub async fn execute(&mut self, sql: &str) -> anyhow::Result<Vec<StatementResult>> {
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("MySQL session is closed"))?;
        CrudService::execute_multi_query_on_conn(conn, &self.database, sql).await
    }

    pub async fn close(mut self) {
        if let Some(mut conn) = self.conn.take() {
            let _ = conn.query_drop("ROLLBACK").await;
            let _ = conn.disconnect().await;
        }
        if let Some(pool) = self.pool.take() {
            let _ = pool.disconnect().await;
        }
    }
}

impl MySqlService {
    /// Create a new MySqlService with a background processing task.
    ///
    /// The service starts without an active connection. Send a `Connect`
    /// command to establish the pool.
    ///
    /// # Arguments
    ///
    /// * `config` - MySQL connection configuration (used for initial connect).
    /// * `tabs` - Tab manager for render notifications.
    /// * `runtime` - Shared tokio runtime handle for spawning the background task.
    /// * `clipboard` - Shared clipboard for copy/paste operations.
    pub fn new(
        _config: MySqlConfig,
        tabs: Arc<dyn TabManager>,
        runtime: tokio::runtime::Handle,
        clipboard: Arc<tokio::sync::RwLock<Option<VoidbClipboard>>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<MySqlCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<MySqlEvent>();

        let task = runtime.spawn(Self::background_task(cmd_rx, event_tx, tabs, clipboard));

        Self {
            mode: ServiceMode::Channel {
                cmd_tx,
                event_rx,
                _task: task,
            },
        }
    }

    /// Create a MySqlService in direct mode for CLI usage.
    ///
    /// Connects immediately, creates inner services, no background task.
    /// Use direct async methods (list_databases, execute_query, etc.) instead
    /// of send/poll_event.
    pub async fn new_direct(config: &MySqlConfig) -> Result<Self, String> {
        Self::new_direct_capability(config)
            .await
            .map_err(|error| capability_error_to_legacy_message(&error))
    }

    /// Create a MySqlService in direct mode with structured diagnostics.
    ///
    /// Capability handlers should use this constructor so connection, auth,
    /// TLS, timeout, and selected-database failures preserve stable categories.
    pub async fn new_direct_capability(config: &MySqlConfig) -> Result<Self, CapabilityError> {
        let violations = config.validate_profile();
        if !violations.is_empty() {
            return Err(mysql_profile_error(config, violations));
        }

        let pool = create_pool(config)
            .map_err(|message| mysql_pool_create_error(config, &message))?;
        // Verify connectivity
        let mut conn = pool
            .get_conn()
            .await
            .map_err(|e| {
                mysql_error_to_capability_error(config, MySqlDiagnosticStage::Connect, &e)
            })?;
        conn.ping()
            .await
            .map_err(|e| {
                mysql_error_to_capability_error(config, MySqlDiagnosticStage::Ping, &e)
            })?;
        drop(conn);

        Ok(Self {
            mode: ServiceMode::Direct {
                schema_svc: SchemaService::new(pool.clone()),
                crud_svc: CrudService::new(pool.clone()),
                _loader_svc: DataLoaderService::new(pool),
            },
        })
    }

    /// Send a command to the background service task (channel mode only).
    ///
    /// This is non-blocking -- safe to call from the synchronous
    /// `Plugin::update()` context.
    pub fn send(&self, cmd: MySqlCommand) {
        if let ServiceMode::Channel { ref cmd_tx, .. } = self.mode {
            let _ = cmd_tx.send(cmd);
        }
    }

    /// Poll for the next event from the service (channel mode only).
    ///
    /// Returns `Some(event)` if available, `None` otherwise.
    /// Non-blocking, suitable for calling from `Plugin::update()`.
    pub fn poll_event(&mut self) -> Option<MySqlEvent> {
        if let ServiceMode::Channel { ref mut event_rx, .. } = self.mode {
            event_rx.try_recv().ok()
        } else {
            None
        }
    }

    // === Direct mode async methods (CLI) ===

    /// List all databases (direct mode).
    pub async fn list_databases(&self) -> anyhow::Result<Vec<String>> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.list_databases().await,
            _ => Err(anyhow::anyhow!("list_databases requires direct mode")),
        }
    }

    /// List tables in a database (direct mode).
    pub async fn list_tables(&self, database: &str) -> anyhow::Result<(Vec<TableMeta>, Vec<String>)> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.list_tables(database).await,
            _ => Err(anyhow::anyhow!("list_tables requires direct mode")),
        }
    }

    /// List columns of a table (direct mode).
    pub async fn list_columns(&self, database: &str, table: &str) -> anyhow::Result<Vec<ColumnInfo>> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.list_columns(database, table).await,
            _ => Err(anyhow::anyhow!("list_columns requires direct mode")),
        }
    }

    /// List indexes of a table (direct mode).
    pub async fn list_indexes(&self, database: &str, table: &str) -> anyhow::Result<Vec<IndexInfo>> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.list_indexes(database, table).await,
            _ => Err(anyhow::anyhow!("list_indexes requires direct mode")),
        }
    }

    /// List foreign keys of a table (direct mode).
    pub async fn list_foreign_keys(&self, database: &str, table: &str) -> anyhow::Result<Vec<ForeignKeyInfo>> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.list_foreign_keys(database, table).await,
            _ => Err(anyhow::anyhow!("list_foreign_keys requires direct mode")),
        }
    }

    /// Show CREATE TABLE DDL (direct mode).
    pub async fn show_create_table(&self, database: &str, table: &str) -> anyhow::Result<String> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.show_create_table(database, table).await,
            _ => Err(anyhow::anyhow!("show_create_table requires direct mode")),
        }
    }

    /// Load table structure: columns, indexes, foreign keys (direct mode).
    pub async fn describe_table(&self, database: &str, table: &str) -> anyhow::Result<(Vec<ColumnInfo>, Vec<IndexInfo>, Vec<ForeignKeyInfo>)> {
        match &self.mode {
            ServiceMode::Direct { schema_svc, .. } => schema_svc.load_structure(database, table).await,
            _ => Err(anyhow::anyhow!("describe_table requires direct mode")),
        }
    }

    /// Execute a SQL query, returning statement results (direct mode).
    pub async fn execute_query(&self, database: &str, sql: &str) -> anyhow::Result<Vec<StatementResult>> {
        match &self.mode {
            ServiceMode::Direct { crud_svc, .. } => crud_svc.execute_multi_query(database, sql).await,
            _ => Err(anyhow::anyhow!("execute_query requires direct mode")),
        }
    }

    /// Background task that processes commands and dispatches to inner services.
    async fn background_task(
        mut cmd_rx: mpsc::UnboundedReceiver<MySqlCommand>,
        event_tx: mpsc::UnboundedSender<MySqlEvent>,
        tabs: Arc<dyn TabManager>,
        clipboard: Arc<tokio::sync::RwLock<Option<VoidbClipboard>>>,
    ) {
        // Pool and inner services are created on Connect
        let mut pool: Option<mysql_async::Pool> = None;
        let mut schema_svc: Option<SchemaService> = None;
        let mut crud_svc: Option<CrudService> = None;
        let mut loader_svc: Option<DataLoaderService> = None;
        let mut browser_svc: Option<BrowserService> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                // === Connection lifecycle ===
                MySqlCommand::Connect { config, reply } => {
                    let violations = config.validate_profile();
                    if !violations.is_empty() {
                        let error = mysql_profile_error(&config, violations);
                        let message = capability_error_to_legacy_message(&error);
                        let _ = reply.send(Err(message.clone()));
                        let _ = event_tx.send(MySqlEvent::Error(message));
                        let _ = tabs.request_render();
                        continue;
                    }

                    match create_pool(&config) {
                        Ok(p) => {
                            // Test connection with a ping
                            let mut conn = match p.get_conn().await {
                                Ok(c) => c,
                                Err(e) => {
                                    let error = mysql_error_to_capability_error(
                                        &config,
                                        MySqlDiagnosticStage::Connect,
                                        &e,
                                    );
                                    let message = capability_error_to_legacy_message(&error);
                                    let _ = reply.send(Err(message.clone()));
                                    let _ = event_tx.send(MySqlEvent::Error(message));
                                    let _ = tabs.request_render();
                                    continue;
                                }
                            };

                            if let Err(e) = conn.ping().await {
                                let error = mysql_error_to_capability_error(
                                    &config,
                                    MySqlDiagnosticStage::Ping,
                                    &e,
                                );
                                let message = capability_error_to_legacy_message(&error);
                                let _ = reply.send(Err(message.clone()));
                                let _ = event_tx.send(MySqlEvent::Error(message));
                                let _ = tabs.request_render();
                                continue;
                            }
                            drop(conn);

                            // Initialize inner services
                            schema_svc = Some(SchemaService::new(p.clone()));
                            crud_svc = Some(CrudService::new(p.clone()));
                            loader_svc = Some(DataLoaderService::new(p.clone()));
                            browser_svc = Some(BrowserService::new(p.clone(), clipboard.clone()));
                            pool = Some(p);

                            let _ = event_tx.send(MySqlEvent::Connected);
                            let _ = tabs.request_render();
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            let error = mysql_pool_create_error(&config, &e);
                            let message = capability_error_to_legacy_message(&error);
                            let _ = reply.send(Err(message.clone()));
                            let _ = event_tx.send(MySqlEvent::Error(message));
                            let _ = tabs.request_render();
                        }
                    }
                }

                MySqlCommand::Ping { reply } => {
                    if let Some(ref p) = pool {
                        match p.get_conn().await {
                            Ok(mut conn) => match conn.ping().await {
                                Ok(()) => {
                                    let _ = reply.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e.to_string()));
                                }
                            },
                            Err(e) => {
                                let _ = reply.send(Err(e.to_string()));
                            }
                        }
                    } else {
                        let _ = reply.send(Err("Not connected".to_string()));
                    }
                }

                MySqlCommand::Disconnect => {
                    // Clean up pool (per Pitfall 2 from RESEARCH.md)
                    if let Some(p) = pool.take() {
                        let _ = p.disconnect().await;
                    }
                    // Drop inner services (they hold pool clones)
                    drop(schema_svc.take());
                    drop(crud_svc.take());
                    drop(loader_svc.take());
                    drop(browser_svc.take());

                    let _ = event_tx.send(MySqlEvent::Disconnected);
                    let _ = tabs.request_render();
                    break;
                }

                // === Schema commands ===
                MySqlCommand::Schema(schema_cmd) => {
                    if let Some(ref svc) = schema_svc {
                        Self::handle_schema(svc, schema_cmd, &event_tx, &tabs).await;
                    } else {
                        let _ = event_tx.send(MySqlEvent::Error("Not connected".into()));
                        let _ = tabs.request_render();
                    }
                }

                // === CRUD commands ===
                MySqlCommand::Crud(crud_cmd) => {
                    if let Some(ref svc) = crud_svc {
                        Self::handle_crud(svc, crud_cmd, &event_tx, &tabs).await;
                    } else {
                        let _ = event_tx.send(MySqlEvent::Error("Not connected".into()));
                        let _ = tabs.request_render();
                    }
                }

                // === Load commands ===
                MySqlCommand::Load(load_cmd) => {
                    if let Some(ref svc) = loader_svc {
                        Self::handle_load(svc, load_cmd, &event_tx, &tabs).await;
                    } else {
                        let _ = event_tx.send(MySqlEvent::Error("Not connected".into()));
                        let _ = tabs.request_render();
                    }
                }

                // === Browser commands ===
                MySqlCommand::Browser(browser_cmd) => {
                    if let Some(ref svc) = browser_svc {
                        svc.handle(browser_cmd, &event_tx, &tabs).await;
                    } else {
                        let _ = event_tx.send(MySqlEvent::Error("Not connected".into()));
                        let _ = tabs.request_render();
                    }
                }
            }
        }

        // Loop exits on sender drop or Disconnect -- ensure pool is cleaned up
        if let Some(p) = pool {
            let _ = p.disconnect().await;
        }
    }

    // === Schema command dispatch ===

    async fn handle_schema(
        svc: &SchemaService,
        cmd: SchemaCommand,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        tabs: &Arc<dyn TabManager>,
    ) {
        match cmd {
            SchemaCommand::ListDatabases => match svc.list_databases().await {
                Ok(dbs) => {
                    let _ = event_tx.send(MySqlEvent::Schema(SchemaEvent::DatabasesLoaded(dbs)));
                }
                Err(e) => {
                    let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                }
            },

            SchemaCommand::ListTables { database } => match svc.list_tables(&database).await {
                Ok((tables, views)) => {
                    let _ = event_tx.send(MySqlEvent::Schema(SchemaEvent::TablesLoaded {
                        database,
                        tables,
                        views,
                    }));
                }
                Err(e) => {
                    let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                }
            },

            SchemaCommand::ListColumns { database, table } => {
                match svc.list_columns(&database, &table).await {
                    Ok(columns) => {
                        let _ =
                            event_tx.send(MySqlEvent::Schema(SchemaEvent::ColumnsLoaded {
                                database,
                                table,
                                columns,
                            }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            SchemaCommand::ListIndexes { database, table } => {
                match svc.list_indexes(&database, &table).await {
                    Ok(indexes) => {
                        let _ =
                            event_tx.send(MySqlEvent::Schema(SchemaEvent::IndexesLoaded {
                                database,
                                table,
                                indexes,
                            }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            SchemaCommand::ShowCreateTable { database, table } => {
                match svc.show_create_table(&database, &table).await {
                    Ok(ddl) => {
                        let _ =
                            event_tx.send(MySqlEvent::Schema(SchemaEvent::CreateTableDdl {
                                database,
                                table,
                                ddl,
                            }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            SchemaCommand::LoadTablesMeta { database } => {
                match svc.load_tables_meta(&database).await {
                    Ok((tables, views)) => {
                        let _ =
                            event_tx.send(MySqlEvent::Schema(SchemaEvent::TablesMetaLoaded {
                                database,
                                tables,
                                views,
                            }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            SchemaCommand::ListForeignKeys { database, table } => {
                match svc.list_foreign_keys(&database, &table).await {
                    Ok(foreign_keys) => {
                        let _ =
                            event_tx.send(MySqlEvent::Schema(SchemaEvent::ForeignKeysLoaded {
                                database,
                                table,
                                foreign_keys,
                            }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            SchemaCommand::LoadStructure { database, table } => {
                match svc.load_structure(&database, &table).await {
                    Ok((columns, indexes, foreign_keys)) => {
                        let _ =
                            event_tx.send(MySqlEvent::Schema(SchemaEvent::StructureLoaded {
                                database,
                                table,
                                columns,
                                indexes,
                                foreign_keys,
                            }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }
        }
        let _ = tabs.request_render();
    }

    // === CRUD command dispatch ===

    async fn handle_crud(
        svc: &CrudService,
        cmd: CrudCommand,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        tabs: &Arc<dyn TabManager>,
    ) {
        match cmd {
            CrudCommand::ExecuteQuery { database, sql } => {
                // Use multi-query for single queries too -- consistent behavior
                match svc.execute_multi_query(&database, &sql).await {
                    Ok(results) => {
                        // If single statement, unwrap to QueryResult for convenience
                        if results.len() == 1 {
                            match results.into_iter().next().unwrap() {
                                StatementResult::Select { columns, rows } => {
                                    let _ = event_tx.send(MySqlEvent::Crud(
                                        CrudEvent::QueryResult { columns, rows },
                                    ));
                                }
                                StatementResult::Affected(n) => {
                                    let _ = event_tx.send(MySqlEvent::Crud(
                                        CrudEvent::MultiQueryResult {
                                            results: vec![StatementResult::Affected(n)],
                                        },
                                    ));
                                }
                                StatementResult::Empty => {
                                    let _ = event_tx.send(MySqlEvent::Crud(
                                        CrudEvent::QueryResult {
                                            columns: vec![],
                                            rows: vec![],
                                        },
                                    ));
                                }
                                StatementResult::Error(e) => {
                                    let _ = event_tx.send(MySqlEvent::Error(e));
                                }
                            }
                        } else {
                            let _ = event_tx.send(MySqlEvent::Crud(
                                CrudEvent::MultiQueryResult { results },
                            ));
                        }
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            CrudCommand::ExecuteMultiQuery { database, sql } => {
                match svc.execute_multi_query(&database, &sql).await {
                    Ok(results) => {
                        let _ = event_tx
                            .send(MySqlEvent::Crud(CrudEvent::MultiQueryResult { results }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            CrudCommand::SaveChanges {
                database,
                table,
                columns,
                rows,
                pk_columns,
                pending,
                deleted,
            } => {
                match svc
                    .save_changes(&database, &table, &columns, &rows, &pk_columns, &pending, &deleted)
                    .await
                {
                    Ok((updates, deletes)) => {
                        let _ = event_tx
                            .send(MySqlEvent::Crud(CrudEvent::ChangesSaved { updates, deletes }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            CrudCommand::InsertRow {
                database,
                table,
                columns,
                values,
            } => match svc.insert_row(&database, &table, &columns, &values).await {
                Ok(sql) => {
                    let _ = event_tx.send(MySqlEvent::Crud(CrudEvent::RowInserted { sql }));
                }
                Err(e) => {
                    let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                }
            },
        }
        let _ = tabs.request_render();
    }

    // === Load command dispatch ===

    async fn handle_load(
        svc: &DataLoaderService,
        cmd: LoadCommand,
        event_tx: &mpsc::UnboundedSender<MySqlEvent>,
        tabs: &Arc<dyn TabManager>,
    ) {
        match cmd {
            LoadCommand::LoadTableData {
                database,
                table,
                page_size,
                offset,
            } => {
                // Load PKs, row count, and data in sequence
                let pks = svc
                    .load_primary_keys(&database, &table)
                    .await
                    .unwrap_or_default();
                let total = svc
                    .load_row_count(&database, &table)
                    .await
                    .unwrap_or(0);

                match svc.load_table_data(&database, &table, page_size, offset).await {
                    Ok((columns, rows)) => {
                        let _ = event_tx.send(MySqlEvent::Load(LoadEvent::TableDataLoaded {
                            database,
                            table,
                            pks,
                            columns,
                            rows,
                            total,
                        }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            LoadCommand::LoadRowCount { database, table } => {
                match svc.load_row_count(&database, &table).await {
                    Ok(count) => {
                        let _ = event_tx.send(MySqlEvent::Load(LoadEvent::RowCountLoaded {
                            database,
                            table,
                            count,
                        }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }

            LoadCommand::LoadPrimaryKeys { database, table } => {
                match svc.load_primary_keys(&database, &table).await {
                    Ok(pks) => {
                        let _ = event_tx.send(MySqlEvent::Load(LoadEvent::PrimaryKeysLoaded {
                            database,
                            table,
                            pks,
                        }));
                    }
                    Err(e) => {
                        let _ = event_tx.send(MySqlEvent::Error(e.to_string()));
                    }
                }
            }
        }
        let _ = tabs.request_render();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // === Send/Sync compile-time assertions ===

    fn assert_send<T: Send>() {}

    #[allow(dead_code)]
    fn assert_sync<T: Sync>() {}

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn test_mysql_command_is_send() {
        assert_send::<MySqlCommand>();
    }

    #[test]
    fn test_mysql_event_is_send() {
        assert_send::<MySqlEvent>();
    }

    #[test]
    fn test_mysql_service_is_send() {
        assert_send::<MySqlService>();
    }

    #[test]
    fn test_mysql_service_in_mutex_is_sync() {
        // MySqlService is Send but NOT Sync (UnboundedReceiver is !Sync).
        // Wrapping in Mutex<T> provides Sync when T: Send.
        // This proves the Plugin: Send + Sync pattern works.
        assert_send_sync::<std::sync::Mutex<MySqlService>>();
    }
}
