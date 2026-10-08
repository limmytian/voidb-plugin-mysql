//! MySQL service event types.
//!
//! Events are sent from the background service task back to the TUI plugin
//! via an unbounded mpsc channel. The TUI polls events via
//! `MySqlService::poll_event()` in its synchronous `Plugin::update()`.

use voidb_core::{ColumnInfo, Row};
use voidb_core::database::types::ForeignKeyInfo;

/// Top-level event enum for the MySQL service.
///
/// Connection lifecycle events are at the top level.
/// Domain events are grouped into sub-enums matching the command domains.
#[derive(Debug)]
pub enum MySqlEvent {
    // --- Connection lifecycle ---
    /// Pool created and ping succeeded.
    Connected,

    /// Pool disconnected cleanly.
    Disconnected,

    // --- Domain events ---
    /// Schema introspection results.
    Schema(SchemaEvent),

    /// CRUD operation results.
    Crud(CrudEvent),

    /// Data loading results.
    Load(LoadEvent),

    /// Browser operation results.
    Browser(BrowserEvent),

    /// An error occurred during an operation (per D-12).
    Error(String),
}

/// Schema introspection events.
#[derive(Debug)]
pub enum SchemaEvent {
    /// List of databases on the server.
    DatabasesLoaded(Vec<String>),

    /// Tables and views in a database.
    TablesLoaded {
        database: String,
        tables: Vec<TableMeta>,
        views: Vec<String>,
    },

    /// Column metadata for a table.
    ColumnsLoaded {
        database: String,
        table: String,
        columns: Vec<ColumnInfo>,
    },

    /// Index metadata for a table.
    IndexesLoaded {
        database: String,
        table: String,
        indexes: Vec<IndexInfo>,
    },

    /// CREATE TABLE DDL for a table.
    CreateTableDdl {
        database: String,
        table: String,
        ddl: String,
    },

    /// Full table metadata for browser overview.
    TablesMetaLoaded {
        database: String,
        tables: Vec<TableMeta>,
        views: Vec<String>,
    },

    /// Foreign keys for a table.
    ForeignKeysLoaded {
        database: String,
        table: String,
        foreign_keys: Vec<ForeignKeyInfo>,
    },

    /// Combined structure info (columns + indexes + foreign keys) for the structure viewer.
    StructureLoaded {
        database: String,
        table: String,
        columns: Vec<ColumnInfo>,
        indexes: Vec<IndexInfo>,
        foreign_keys: Vec<ForeignKeyInfo>,
    },
}

/// CRUD operation events.
#[derive(Debug)]
pub enum CrudEvent {
    /// Single query result (columns + rows).
    QueryResult {
        columns: Vec<ColumnInfo>,
        rows: Vec<Row>,
    },

    /// Multiple statement results.
    MultiQueryResult { results: Vec<StatementResult> },

    /// Cell edits and row deletions applied.
    ChangesSaved { updates: usize, deletes: usize },

    /// A new row was inserted.
    RowInserted { sql: String },
}

/// Result of a single SQL statement within a multi-query execution.
///
/// Mirrors `data_loader_async::StatementResult` but is owned by the service
/// layer, decoupling events from the legacy module.
#[derive(Debug)]
pub enum StatementResult {
    /// A SELECT/SHOW/DESCRIBE/EXPLAIN result.
    Select {
        columns: Vec<ColumnInfo>,
        rows: Vec<Row>,
    },
    /// An INSERT/UPDATE/DELETE result with affected row count.
    Affected(u64),
    /// Empty result set.
    Empty,
    /// Statement execution error.
    Error(String),
}

/// Data loading events.
#[derive(Debug)]
pub enum LoadEvent {
    /// A page of table data with primary keys and total row count.
    TableDataLoaded {
        database: String,
        table: String,
        pks: Vec<String>,
        columns: Vec<ColumnInfo>,
        rows: Vec<Row>,
        total: u64,
    },

    /// Total row count for a table.
    RowCountLoaded {
        database: String,
        table: String,
        count: u64,
    },

    /// Primary key column names for a table.
    PrimaryKeysLoaded {
        database: String,
        table: String,
        pks: Vec<String>,
    },
}

/// Browser / DDL operation events.
#[derive(Debug)]
pub enum BrowserEvent {
    /// Database created successfully.
    DatabaseCreated { name: String },

    /// Database dropped successfully.
    DatabaseDropped { name: String },

    /// Table dropped successfully.
    TableDropped { database: String, table: String },

    /// Table renamed successfully.
    TableRenamed {
        database: String,
        old_name: String,
        new_name: String,
    },

    /// Table duplicated successfully.
    TableDuplicated {
        database: String,
        source: String,
        target: String,
    },

    /// Copy-tables operation started.
    CopyTablesStarted,

    /// Progress update for copy-tables.
    CopyTablesProgress { table: String, rows: usize },

    /// Copy-tables operation completed.
    CopyTablesDone,

    /// Raw SQL executed in browser context.
    SqlExecuted { result: String },

    /// Columns loaded for the schema editor.
    ColumnsForEditorLoaded {
        database: String,
        table: String,
        columns: Vec<ColumnInfo>,
    },

    /// Schema changes applied from the editor.
    SchemaChangesApplied { database: String, table: String },

    /// Progress update for paste-database.
    PasteDatabaseProgress { table: String, rows: usize },

    /// Paste-database operation completed.
    PasteDatabaseDone,
}

/// Table metadata for browser display.
#[derive(Debug, Clone)]
pub struct TableMeta {
    /// Table name.
    pub name: String,
    /// Table type (e.g., "BASE TABLE", "VIEW").
    pub table_type: String,
    /// Approximate row count (from INFORMATION_SCHEMA).
    pub rows: Option<u64>,
    /// Table comment.
    pub comment: Option<String>,
}

/// Index metadata.
#[derive(Debug, Clone)]
pub struct IndexInfo {
    /// Index name.
    pub name: String,
    /// Columns in the index.
    pub columns: Vec<String>,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// Index type (e.g., "BTREE", "FULLTEXT").
    pub index_type: String,
}
