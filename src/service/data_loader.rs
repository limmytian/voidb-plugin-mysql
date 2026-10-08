//! Data loading inner service.
//!
//! Handles `LoadCommand` variants: paginated table data loading,
//! row count queries, and primary key discovery.

use anyhow::Result;
use mysql_async::Pool;
use mysql_async::prelude::*;
use voidb_core::{ColumnInfo, Row};

use super::value_convert::{convert_row, extract_columns};

/// Inner service for data loading operations.
///
/// Holds a reference to the connection pool and provides methods
/// corresponding to each `LoadCommand` variant.
pub struct DataLoaderService {
    pool: Pool,
}

impl DataLoaderService {
    /// Create a new data loader service backed by the given pool.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Load a page of table data.
    ///
    /// Returns column metadata and rows for the given offset/limit.
    pub async fn load_table_data(
        &self,
        database: &str,
        table: &str,
        limit: usize,
        offset: u64,
    ) -> Result<(Vec<ColumnInfo>, Vec<Row>)> {
        let mut conn = self.pool.get_conn().await?;

        conn.query_drop(format!("USE `{}`", database)).await?;

        let query = format!(
            "SELECT * FROM `{}` LIMIT {} OFFSET {}",
            table, limit, offset
        );

        let result: Vec<mysql_async::Row> = conn.query(query).await?;

        if result.is_empty() {
            // Still need columns even when page is empty
            let meta_query = format!("SELECT * FROM `{}` LIMIT 0", table);
            let meta_result: Vec<mysql_async::Row> = conn.query(meta_query).await?;
            if meta_result.is_empty() {
                return Ok((Vec::new(), Vec::new()));
            }
            let columns = extract_columns(&meta_result[0])?;
            return Ok((columns, Vec::new()));
        }

        let columns = extract_columns(&result[0])?;
        let data_rows: Vec<Row> = result
            .iter()
            .map(|row| convert_row(row, columns.len()))
            .collect();

        Ok((columns, data_rows))
    }

    /// Load the total row count for a table.
    pub async fn load_row_count(&self, database: &str, table: &str) -> Result<u64> {
        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        let count: Option<u64> = conn
            .query_first(format!("SELECT COUNT(*) FROM `{}`", table))
            .await?;

        Ok(count.unwrap_or(0))
    }

    /// Load primary key column names for a table.
    pub async fn load_primary_keys(
        &self,
        database: &str,
        table: &str,
    ) -> Result<Vec<String>> {
        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        let rows: Vec<mysql_async::Row> = conn
            .query(format!(
                "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = '{}' AND TABLE_NAME = '{}' AND CONSTRAINT_NAME = 'PRIMARY' \
                 ORDER BY ORDINAL_POSITION",
                database.replace('\'', "''"),
                table.replace('\'', "''"),
            ))
            .await?;

        let pks: Vec<String> = rows
            .iter()
            .filter_map(|r| r.get::<String, _>(0))
            .collect();

        Ok(pks)
    }
}
