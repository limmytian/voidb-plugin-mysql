//! Schema introspection inner service.
//!
//! Handles `SchemaCommand` variants: listing databases, tables, columns,
//! indexes, DDL, and full table metadata for browser overview.

use std::collections::HashMap;

use anyhow::Result;
use mysql_async::Pool;
use mysql_async::prelude::*;
use voidb_core::ColumnInfo;

use voidb_core::database::types::ForeignKeyInfo;

use super::events::{IndexInfo, TableMeta};

/// Inner service for schema introspection.
pub struct SchemaService {
    pool: Pool,
}

impl SchemaService {
    /// Create a new schema service backed by the given pool.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// List all databases on the server.
    pub async fn list_databases(&self) -> Result<Vec<String>> {
        let mut conn = self.pool.get_conn().await?;
        let databases: Vec<String> = conn.query("SHOW DATABASES").await?;
        Ok(databases)
    }

    /// List tables and views in a database.
    ///
    /// Returns `(tables, views)` where tables include metadata from
    /// INFORMATION_SCHEMA and views are just names.
    pub async fn list_tables(&self, database: &str) -> Result<(Vec<TableMeta>, Vec<String>)> {
        let mut conn = self.pool.get_conn().await?;

        let sql = "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, TABLE_COMMENT \
                   FROM INFORMATION_SCHEMA.TABLES \
                   WHERE TABLE_SCHEMA = ? \
                   ORDER BY TABLE_NAME";

        let rows: Vec<(String, String, Option<u64>, Option<String>)> =
            conn.exec(sql, (database,)).await?;

        let mut tables = Vec::new();
        let mut views = Vec::new();

        for (name, table_type, row_count, comment) in rows {
            if table_type == "VIEW" {
                views.push(name);
            } else {
                tables.push(TableMeta {
                    name,
                    table_type,
                    rows: row_count,
                    comment,
                });
            }
        }

        Ok((tables, views))
    }

    /// List columns for a specific table.
    pub async fn list_columns(&self, database: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        let rows: Vec<mysql_async::Row> = conn
            .query(format!("SHOW FULL COLUMNS FROM `{}`", table))
            .await?;

        let columns = rows
            .iter()
            .map(|row| {
                let name = row_string(row, 0);
                let data_type = row_string(row, 1);
                let nullable_str = row_string(row, 3);
                let key = row_string(row, 4);
                let default_value = row_optional_string(row, 5);
                let extra = row_string(row, 6);

                ColumnInfo {
                    name,
                    data_type,
                    nullable: nullable_str == "YES",
                    is_primary_key: key == "PRI",
                    default_value,
                    max_length: None,
                    extra,
                }
            })
            .collect();

        Ok(columns)
    }

    /// List indexes for a specific table.
    pub async fn list_indexes(&self, database: &str, table: &str) -> Result<Vec<IndexInfo>> {
        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        let rows: Vec<mysql_async::Row> =
            conn.query(format!("SHOW INDEX FROM `{}`", table)).await?;

        let mut index_map: HashMap<String, IndexInfo> = HashMap::new();

        for row in &rows {
            let name = row_string(row, 2); // Key_name
            let column_name = row_string(row, 4); // Column_name
            let non_unique = row_i64(row, 1, 1); // Non_unique
            let index_type = row_string(row, 10); // Index_type

            index_map
                .entry(name.clone())
                .or_insert_with(|| IndexInfo {
                    name,
                    columns: Vec::new(),
                    unique: non_unique == 0,
                    index_type,
                })
                .columns
                .push(column_name);
        }

        Ok(index_map.into_values().collect())
    }

    /// Get the CREATE TABLE DDL for a table.
    pub async fn show_create_table(&self, database: &str, table: &str) -> Result<String> {
        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        let row: Option<mysql_async::Row> = conn
            .query_first(format!("SHOW CREATE TABLE `{}`", table))
            .await?;

        match row {
            Some(r) => {
                let ddl: String = r.get(1).unwrap_or_default();
                Ok(ddl)
            }
            None => Err(anyhow::anyhow!(
                "No DDL returned for {}.{}",
                database,
                table
            )),
        }
    }

    /// Load full table metadata for browser overview.
    ///
    /// Same as `list_tables` -- the browser uses this for its overview panel.
    pub async fn load_tables_meta(&self, database: &str) -> Result<(Vec<TableMeta>, Vec<String>)> {
        self.list_tables(database).await
    }

    /// List foreign keys for a specific table.
    pub async fn list_foreign_keys(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<ForeignKeyInfo>> {
        let mut conn = self.pool.get_conn().await?;

        let sql = "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_NAME, \
                   k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
                   FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE k \
                   LEFT JOIN INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS r \
                       ON k.CONSTRAINT_NAME = r.CONSTRAINT_NAME \
                       AND k.CONSTRAINT_SCHEMA = r.CONSTRAINT_SCHEMA \
                   WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? \
                     AND k.REFERENCED_TABLE_NAME IS NOT NULL \
                   ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION";

        let rows: Vec<mysql_async::Row> = conn.exec(sql, (database, table)).await?;

        let mut fk_map: HashMap<String, ForeignKeyInfo> = HashMap::new();

        for r in &rows {
            let name = row_string(r, 0);
            let col = row_string(r, 1);
            let ref_table = row_string(r, 2);
            let ref_col = row_string(r, 3);
            let on_update = row_string(r, 4);
            let on_delete = row_string(r, 5);

            let entry = fk_map
                .entry(name.clone())
                .or_insert_with(|| ForeignKeyInfo {
                    name,
                    columns: Vec::new(),
                    referenced_table: ref_table,
                    referenced_columns: Vec::new(),
                    on_update,
                    on_delete,
                });
            entry.columns.push(col);
            entry.referenced_columns.push(ref_col);
        }

        Ok(fk_map.into_values().collect())
    }

    /// Load full structure info (columns + indexes + foreign keys) for the structure viewer.
    ///
    /// Combines three queries in a single service call to avoid the TUI needing
    /// to coordinate multiple async responses.
    pub async fn load_structure(
        &self,
        database: &str,
        table: &str,
    ) -> Result<(Vec<ColumnInfo>, Vec<IndexInfo>, Vec<ForeignKeyInfo>)> {
        let columns = self.list_columns(database, table).await?;
        let indexes = self.list_indexes(database, table).await?;
        let foreign_keys = self.list_foreign_keys(database, table).await?;
        Ok((columns, indexes, foreign_keys))
    }
}

fn row_string(row: &mysql_async::Row, index: usize) -> String {
    row.get_opt::<String, _>(index)
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn row_optional_string(row: &mysql_async::Row, index: usize) -> Option<String> {
    row.get_opt::<String, _>(index).and_then(Result::ok)
}

fn row_i64(row: &mysql_async::Row, index: usize, default: i64) -> i64 {
    row.get_opt::<i64, _>(index)
        .and_then(Result::ok)
        .unwrap_or(default)
}
