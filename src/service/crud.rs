//! CRUD inner service.
//!
//! Handles `CrudCommand` variants: single query execution, multi-query
//! execution, saving pending changes (UPDATE/DELETE), and inserting rows.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use mysql_async::{Conn, Pool};
use mysql_async::prelude::*;
use voidb_core::{CellValue, ColumnInfo, Row};

use super::events::StatementResult;
use super::value_convert::{build_pk_where, convert_row, escape_value, extract_columns};

/// Inner service for CRUD operations.
pub struct CrudService {
    pool: Pool,
}

impl CrudService {
    /// Create a new CRUD service backed by the given pool.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Execute multiple SQL statements sequentially on a single connection.
    ///
    /// Returns one `StatementResult` per statement. Stops on first error.
    pub async fn execute_multi_query(
        &self,
        database: &str,
        sql: &str,
    ) -> Result<Vec<StatementResult>> {
        let mut conn = self.pool.get_conn().await?;
        Self::execute_multi_query_on_conn(&mut conn, database, sql).await
    }

    /// Execute statements on an already-owned connection so transaction,
    /// temporary-table, prepared, lock, and session-variable state survives.
    pub(crate) async fn execute_multi_query_on_conn(
        conn: &mut Conn,
        database: &str,
        sql: &str,
    ) -> Result<Vec<StatementResult>> {
        let stmts = voidb_core::sql_split::split_statements(sql);
        if stmts.is_empty() {
            return Ok(vec![]);
        }
        if !database.is_empty() {
            conn.query_drop(format!("USE `{}`", database)).await?;
        }

        let mut results = Vec::new();
        for stmt in stmts {
            let trimmed = stmt.trim_start().to_uppercase();
            let is_select = trimmed.starts_with("SELECT")
                || trimmed.starts_with("SHOW")
                || trimmed.starts_with("DESCRIBE")
                || trimmed.starts_with("DESC ")
                || trimmed.starts_with("EXPLAIN");

            if is_select {
                match conn.query::<mysql_async::Row, _>(stmt).await {
                    Ok(rows) if rows.is_empty() => {
                        results.push(StatementResult::Empty);
                    }
                    Ok(rows) => match extract_columns(&rows[0]) {
                        Ok(columns) => {
                            let data_rows: Vec<Row> = rows
                                .iter()
                                .map(|r| convert_row(r, columns.len()))
                                .collect();
                            results.push(StatementResult::Select {
                                columns,
                                rows: data_rows,
                            });
                        }
                        Err(e) => {
                            results.push(StatementResult::Error(e.to_string()));
                            break;
                        }
                    },
                    Err(e) => {
                        results.push(StatementResult::Error(e.to_string()));
                        break;
                    }
                }
            } else {
                match conn.query_iter(stmt).await {
                    Ok(result) => {
                        let affected = result.affected_rows();
                        drop(result);
                        results.push(StatementResult::Affected(affected));
                    }
                    Err(e) => {
                        results.push(StatementResult::Error(e.to_string()));
                        break;
                    }
                }
            }
        }
        Ok(results)
    }

    /// Save pending cell edits and row deletions.
    ///
    /// Generates UPDATE statements for modified cells and DELETE statements
    /// for deleted rows, then executes them on a single connection.
    ///
    /// Returns `(update_count, delete_count)`.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_changes(
        &self,
        database: &str,
        table: &str,
        columns: &[ColumnInfo],
        rows: &[Row],
        pk_cols: &[String],
        pending: &HashMap<(usize, usize), CellValue>,
        deleted_rows: &[usize],
    ) -> Result<(usize, usize)> {
        if pending.is_empty() && deleted_rows.is_empty() {
            return Ok((0, 0));
        }

        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        let mut sqls = Vec::new();

        // Build UPDATE statements (skip rows that are also deleted)
        let deleted_set: HashSet<usize> = deleted_rows.iter().copied().collect();
        let mut row_changes: HashMap<usize, Vec<(usize, &CellValue)>> = HashMap::new();
        for (&(row, col), val) in pending {
            if !deleted_set.contains(&row) {
                row_changes.entry(row).or_default().push((col, val));
            }
        }

        let mut update_count = 0;
        for (row_idx, changes) in &row_changes {
            let where_clause = build_pk_where(columns, &rows[*row_idx], pk_cols)?;
            let set_parts: Vec<String> = changes
                .iter()
                .map(|(col, val)| format!("`{}` = {}", columns[*col].name, escape_value(val)))
                .collect();
            let sql = format!(
                "UPDATE `{}` SET {} WHERE {}",
                table,
                set_parts.join(", "),
                where_clause,
            );
            sqls.push(sql);
            update_count += 1;
        }

        // Build DELETE statements
        let mut delete_count = 0;
        for &row_idx in deleted_rows {
            if row_idx < rows.len() {
                let where_clause = build_pk_where(columns, &rows[row_idx], pk_cols)?;
                let sql = format!("DELETE FROM `{}` WHERE {}", table, where_clause);
                sqls.push(sql);
                delete_count += 1;
            }
        }

        for sql in &sqls {
            conn.query_drop(sql.as_str()).await?;
        }

        Ok((update_count, delete_count))
    }

    /// Insert a new row. Returns the SQL statement used.
    pub async fn insert_row(
        &self,
        database: &str,
        table: &str,
        columns: &[ColumnInfo],
        values: &[CellValue],
    ) -> Result<String> {
        let mut conn = self.pool.get_conn().await?;
        conn.query_drop(format!("USE `{}`", database)).await?;

        // Skip columns where value is Null and the column can be omitted
        let pairs: Vec<(&ColumnInfo, &CellValue)> = columns
            .iter()
            .zip(values.iter())
            .filter(|(col, val)| {
                if !matches!(val, CellValue::Null) {
                    return true;
                }
                let is_auto = col.extra.to_lowercase().contains("auto_increment");
                let has_default = col.default_value.is_some();
                let is_nullable = col.nullable;
                !(is_auto || has_default || is_nullable)
            })
            .collect();

        let col_names: Vec<String> = pairs.iter().map(|(c, _)| format!("`{}`", c.name)).collect();
        let val_strs: Vec<String> = pairs.iter().map(|(_, v)| escape_value(v)).collect();

        let sql = if col_names.is_empty() {
            format!("INSERT INTO `{}` VALUES ()", table)
        } else {
            format!(
                "INSERT INTO `{}` ({}) VALUES ({})",
                table,
                col_names.join(", "),
                val_strs.join(", "),
            )
        };

        conn.query_drop(sql.as_str()).await?;
        Ok(sql)
    }
}
