//! Centralized MySQL value conversion utilities.
//!
//! This module provides pure functions for converting between `mysql_async::Value`
//! and `voidb_core::CellValue`, as well as SQL generation helpers (`escape_value`,
//! `build_pk_where`). These are shared by the CRUD and data-loading inner services,
//! eliminating the value-conversion duplication that previously existed across
//! `data_loader_async.rs` and `lib.rs`.

use anyhow::Result;
use voidb_core::{CellValue, ColumnInfo, Row};

/// Convert a single `mysql_async::Value` (from a Row column) to `CellValue`.
///
/// # Arguments
///
/// * `row` - The mysql_async Row containing the value.
/// * `index` - Column index within the row.
pub fn convert_value(row: &mysql_async::Row, index: usize) -> CellValue {
    use mysql_async::Value;

    match row.as_ref(index) {
        Some(value) => match value {
            Value::NULL => CellValue::Null,
            Value::Bytes(bytes) => match String::from_utf8(bytes.clone()) {
                Ok(text) => CellValue::Text(text),
                Err(_) => CellValue::Blob(bytes.clone()),
            },
            Value::Int(i) => CellValue::Int(*i),
            Value::UInt(u) => CellValue::Int(*u as i64),
            Value::Float(f) => CellValue::Float(*f as f64),
            Value::Double(d) => CellValue::Float(*d),
            Value::Date(year, month, day, hour, min, sec, _micro) => {
                if *hour == 0 && *min == 0 && *sec == 0 {
                    CellValue::Date(format!("{:04}-{:02}-{:02}", year, month, day))
                } else {
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
        },
        None => CellValue::Null,
    }
}

/// Extract column metadata from a `mysql_async::Row`.
///
/// Returns column names and types based on the row's column metadata.
/// Note: nullable, primary key, default, and extra info are not available
/// from row metadata alone -- these are populated from INFORMATION_SCHEMA queries.
pub fn extract_columns(row: &mysql_async::Row) -> Result<Vec<ColumnInfo>> {
    let columns = row.columns();

    Ok(columns
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
        .collect())
}

/// Convert a `mysql_async::Row` to a `voidb_core::Row`.
pub fn convert_row(row: &mysql_async::Row, col_count: usize) -> Row {
    let values: Vec<CellValue> = (0..col_count).map(|i| convert_value(row, i)).collect();
    Row { values }
}

// ---------------------------------------------------------------------------
// SQL generation helpers
// ---------------------------------------------------------------------------

/// Escape a `CellValue` for use in raw SQL statements.
///
/// This produces a SQL literal suitable for embedding in generated
/// UPDATE/DELETE/INSERT statements. Strings are single-quote escaped.
pub fn escape_value(v: &CellValue) -> String {
    match v {
        CellValue::Null => "NULL".into(),
        CellValue::Int(i) => i.to_string(),
        CellValue::Float(f) => f.to_string(),
        CellValue::Bool(b) => if *b { "1" } else { "0" }.into(),
        CellValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
        CellValue::Date(s) | CellValue::Time(s) | CellValue::DateTime(s) => {
            format!("'{}'", s.replace('\'', "''"))
        }
        CellValue::Blob(b) => format!("X'{}'", hex::encode(b)),
        CellValue::Json(s) => format!("'{}'", s.replace('\'', "''")),
        CellValue::Uuid(s) | CellValue::Unknown(s) => format!("'{}'", s.replace('\'', "''")),
    }
}

/// Build a WHERE clause from primary key columns and a data row.
///
/// Produces expressions like `` `id` = 42 AND `name` = 'foo' ``.
/// NULL PKs are handled with `IS NULL`.
///
/// # Errors
///
/// Returns an error if `pk_cols` is empty or a PK column is not found
/// in the column list.
pub fn build_pk_where(columns: &[ColumnInfo], row: &Row, pk_cols: &[String]) -> Result<String> {
    if pk_cols.is_empty() {
        return Err(anyhow::anyhow!("No primary key -- cannot identify row"));
    }

    let mut conditions = Vec::new();
    for pk in pk_cols {
        let idx = columns
            .iter()
            .position(|c| c.name == *pk)
            .ok_or_else(|| anyhow::anyhow!("PK column '{}' not found", pk))?;

        let val = &row.values[idx];
        if matches!(val, CellValue::Null) {
            conditions.push(format!("`{}` IS NULL", pk));
        } else {
            conditions.push(format!("`{}` = {}", pk, escape_value(val)));
        }
    }

    Ok(conditions.join(" AND "))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_value_null() {
        assert_eq!(escape_value(&CellValue::Null), "NULL");
    }

    #[test]
    fn test_escape_value_int() {
        assert_eq!(escape_value(&CellValue::Int(42)), "42");
    }

    #[test]
    fn test_escape_value_text_with_quotes() {
        assert_eq!(escape_value(&CellValue::Text("it's".into())), "'it''s'");
    }

    #[test]
    fn test_escape_value_blob() {
        assert_eq!(
            escape_value(&CellValue::Blob(vec![0xDE, 0xAD])),
            "X'dead'"
        );
    }

    #[test]
    fn test_escape_value_float() {
        // Float should produce a numeric string
        let result = escape_value(&CellValue::Float(12.5));
        assert!(result.starts_with("12.5"));
    }

    #[test]
    fn test_escape_value_bool() {
        assert_eq!(escape_value(&CellValue::Bool(true)), "1");
        assert_eq!(escape_value(&CellValue::Bool(false)), "0");
    }

    #[test]
    fn test_escape_value_datetime() {
        assert_eq!(
            escape_value(&CellValue::DateTime("2024-01-15 10:30:00".into())),
            "'2024-01-15 10:30:00'"
        );
    }

    #[test]
    fn test_build_pk_where_single() {
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "INT".into(),
            nullable: false,
            is_primary_key: true,
            default_value: None,
            max_length: None,
            extra: String::new(),
        }];
        let row = Row {
            values: vec![CellValue::Int(42)],
        };
        let result = build_pk_where(&columns, &row, &["id".into()]).unwrap();
        assert_eq!(result, "`id` = 42");
    }

    #[test]
    fn test_build_pk_where_composite() {
        let columns = vec![
            ColumnInfo {
                name: "a".into(),
                data_type: "INT".into(),
                nullable: false,
                is_primary_key: true,
                default_value: None,
                max_length: None,
                extra: String::new(),
            },
            ColumnInfo {
                name: "b".into(),
                data_type: "VARCHAR".into(),
                nullable: false,
                is_primary_key: true,
                default_value: None,
                max_length: None,
                extra: String::new(),
            },
        ];
        let row = Row {
            values: vec![CellValue::Int(1), CellValue::Text("x".into())],
        };
        let result = build_pk_where(&columns, &row, &["a".into(), "b".into()]).unwrap();
        assert_eq!(result, "`a` = 1 AND `b` = 'x'");
    }

    #[test]
    fn test_build_pk_where_null_pk() {
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "INT".into(),
            nullable: true,
            is_primary_key: true,
            default_value: None,
            max_length: None,
            extra: String::new(),
        }];
        let row = Row {
            values: vec![CellValue::Null],
        };
        let result = build_pk_where(&columns, &row, &["id".into()]).unwrap();
        assert_eq!(result, "`id` IS NULL");
    }

    #[test]
    fn test_build_pk_where_no_pks() {
        let columns = vec![ColumnInfo {
            name: "id".into(),
            data_type: "INT".into(),
            nullable: false,
            is_primary_key: true,
            default_value: None,
            max_length: None,
            extra: String::new(),
        }];
        let row = Row {
            values: vec![CellValue::Int(1)],
        };
        let result = build_pk_where(&columns, &row, &[]);
        assert!(result.is_err());
    }
}
