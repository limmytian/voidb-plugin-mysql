//! MySQL service command types.
//!
//! Commands are sent from the TUI plugin to the background service task
//! via an unbounded mpsc channel. Each domain (schema, CRUD, data loading,
//! browser) has its own sub-enum for organization.

use std::collections::HashMap;

use tokio::sync::oneshot;
use voidb_core::{CellValue, ColumnInfo, Row};

use crate::config::MySqlConfig;

/// Top-level command enum for the MySQL service.
///
/// Connection lifecycle commands are at the top level.
/// Domain-specific commands are grouped into sub-enums.
#[derive(Debug)]
pub enum MySqlCommand {
    // --- Connection lifecycle ---
    /// Establish a connection pool from the given config.
    /// The oneshot reply signals success or failure.
    Connect {
        config: MySqlConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },

    /// Ping the server to verify the connection is alive.
    Ping {
        reply: oneshot::Sender<Result<(), String>>,
    },

    /// Disconnect and shut down the background task.
    Disconnect,

    // --- Domain sub-commands ---
    /// Schema introspection operations.
    Schema(SchemaCommand),

    /// CRUD operations (query, execute, save, insert).
    Crud(CrudCommand),

    /// Data loading operations (pagination, row count, PKs).
    Load(LoadCommand),

    /// Browser operations (DDL, table management).
    Browser(BrowserCommand),
}

/// Schema introspection commands.
#[derive(Debug)]
pub enum SchemaCommand {
    /// List all databases on the server.
    ListDatabases,

    /// List tables in a database.
    ListTables { database: String },

    /// List columns for a specific table.
    ListColumns { database: String, table: String },

    /// List indexes for a specific table.
    ListIndexes { database: String, table: String },

    /// Get the CREATE TABLE DDL for a table.
    ShowCreateTable { database: String, table: String },

    /// Load full table metadata (columns, types, PKs, comments, row counts).
    LoadTablesMeta { database: String },

    /// Load foreign keys for a specific table.
    ListForeignKeys { database: String, table: String },

    /// Load full structure info (columns + indexes + foreign keys) for the structure viewer.
    LoadStructure { database: String, table: String },
}

/// CRUD commands (query execution and data mutation).
#[derive(Debug)]
pub enum CrudCommand {
    /// Execute a single SQL query and return results.
    ExecuteQuery { database: String, sql: String },

    /// Execute multiple SQL statements sequentially.
    ExecuteMultiQuery { database: String, sql: String },

    /// Save pending cell edits and row deletions.
    SaveChanges {
        database: String,
        table: String,
        columns: Vec<ColumnInfo>,
        rows: Vec<Row>,
        pk_columns: Vec<String>,
        pending: HashMap<(usize, usize), CellValue>,
        deleted: Vec<usize>,
    },

    /// Insert a new row into a table.
    InsertRow {
        database: String,
        table: String,
        columns: Vec<ColumnInfo>,
        values: Vec<CellValue>,
    },
}

/// Data loading commands (pagination, counts, PKs).
#[derive(Debug)]
pub enum LoadCommand {
    /// Load a page of table data.
    LoadTableData {
        database: String,
        table: String,
        page_size: usize,
        offset: u64,
    },

    /// Load the total row count for a table.
    LoadRowCount { database: String, table: String },

    /// Load primary key column names for a table.
    LoadPrimaryKeys { database: String, table: String },
}

/// Browser / DDL management commands.
#[derive(Debug)]
pub enum BrowserCommand {
    /// Create a new database.
    CreateDatabase { name: String },

    /// Drop an existing database.
    DropDatabase { name: String },

    /// Drop a table from a database.
    DropTable { database: String, table: String },

    /// Rename a table.
    RenameTable {
        database: String,
        old_name: String,
        new_name: String,
    },

    /// Duplicate a table (structure + optionally data).
    DuplicateTable {
        database: String,
        source: String,
        target: String,
    },

    /// Copy tables from a source to internal clipboard.
    CopyTables {
        source_url: String,
        connection_id: String,
        database: String,
        tables: Vec<String>,
        with_data: bool,
    },

    /// Copy an entire database to internal clipboard.
    CopyDatabase {
        source_url: String,
        connection_id: String,
        database: String,
        with_data: bool,
    },

    /// Paste previously copied tables into a target database.
    PasteTables {
        target_database: String,
        table_schemas: Vec<TableCopyData>,
    },

    /// Execute raw SQL (for browser-level operations).
    ExecuteSql { sql: String },

    /// Load columns for the schema editor.
    LoadColumnsForEditor { database: String, table: String },

    /// Apply schema changes from the schema editor.
    ApplySchemaChanges {
        database: String,
        table: String,
        sql: Vec<String>,
    },

    /// Paste an entire database (all tables from clipboard).
    PasteDatabase {
        target_database: String,
        source_url: String,
        source_database: String,
        tables: Vec<String>,
        with_data: bool,
    },
}

/// Data needed for a table copy/paste operation.
#[derive(Debug, Clone)]
pub struct TableCopyData {
    /// Name of the table to create.
    pub table_name: String,
    /// CREATE TABLE SQL statement.
    pub create_sql: String,
    /// Connection URL for the source database (to copy data from).
    pub source_url: String,
    /// Whether to copy data in addition to structure.
    pub with_data: bool,
}
