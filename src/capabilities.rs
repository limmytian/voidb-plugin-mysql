#![allow(clippy::result_large_err)]

use serde_json::{Value, json};
use voidb_core::formatters::cell_value_to_json;
use voidb_core::{
    CapabilityDefinition, CapabilityError, CapabilityErrorCategory, CapabilityInvocation,
    CapabilityInvocationResult, CapabilityRiskLevel, CredentialClass, InvocationOutputPage,
    InvocationStatus, LocalPathError, LocalPathScope, Pagination, RedactionStatus,
    SQL_DEFAULT_ROW_LIMIT, SQL_MAX_ROW_LIMIT, SQL_RESULT_CONTRACT_VERSION, SqlCatalogScope,
    SqlDataFormat, SqlDialect, SqlInputScope, SqlIntrospectionSupport, TargetSystemFailure,
    build_sql_import_batch, encode_sql_export, parse_sql_import_page, sql_allows_read_only_query,
    sql_catalog_entry, sql_catalogs_input_schema, sql_catalogs_output_schema, sql_column_metadata,
    sql_describe_table_input_schema, sql_describe_table_output_schema, sql_explain_input_schema,
    sql_explain_output_schema, sql_explain_statement, sql_export_input_schema,
    sql_export_output_schema, sql_export_page_query, sql_foreign_key_constraints,
    sql_import_input_schema, sql_import_output_schema, sql_introspection_capabilities,
    sql_mutation_gate, sql_mutation_preview, sql_relation, sql_scope, sql_statement_count,
    sql_statements_output_schema, sql_tables_input_schema, sql_tables_output_schema,
    sql_text_input_schema,
};

use crate::config::MySqlConfig;
use crate::diagnostics::redact_mysql_diagnostic;
use crate::service::{MySqlService, StatementResult};

const PLUGIN_ID: &str = "mysql";
const DEFAULT_ROW_LIMIT: usize = SQL_DEFAULT_ROW_LIMIT;
const MAX_ROW_LIMIT: usize = SQL_MAX_ROW_LIMIT;

pub fn mysql_capabilities() -> Vec<CapabilityDefinition> {
    vec![
        capability(
            "query",
            "Execute read-oriented MySQL SQL and return row sets.",
            sql_text_input_schema(
                "Read-oriented SQL. Mutating statements must use mysql.exec.",
                SqlInputScope::database(false),
            ),
            sql_statements_output_schema(false),
            vec!["connection.read", "sql.query"],
            false,
            false,
            false,
            Some(30_000),
        ),
        capability(
            "explain",
            "Inspect the query plan for one read-oriented MySQL SQL statement.",
            sql_explain_input_schema(SqlInputScope::database(false)),
            sql_explain_output_schema(),
            vec!["connection.read", "sql.explain"],
            false,
            false,
            false,
            Some(30_000),
        ),
        capability(
            "exec",
            "Execute MySQL SQL that may mutate schema or data.",
            sql_text_input_schema(
                "SQL that may mutate MySQL schema or data.",
                SqlInputScope::database(false),
            ),
            sql_statements_output_schema(true),
            vec!["connection.read", "sql.exec"],
            true,
            false,
            true,
            Some(30_000),
        ),
        capability(
            "catalogs",
            "List MySQL databases using a bounded, filterable catalog.",
            sql_catalogs_input_schema(),
            sql_catalogs_output_schema(SqlCatalogScope::Database),
            vec!["connection.read", "schema.list"],
            false,
            false,
            false,
            Some(30_000),
        ),
        capability(
            "tables",
            "List MySQL tables and views for the selected database.",
            sql_tables_input_schema(SqlInputScope::database(false)),
            sql_tables_output_schema(SqlCatalogScope::Database),
            vec!["connection.read", "schema.list"],
            false,
            false,
            false,
            Some(30_000),
        ),
        capability(
            "describe_table",
            "Describe MySQL table columns, indexes, and foreign keys.",
            sql_describe_table_input_schema(SqlInputScope::database(false)),
            sql_describe_table_output_schema(SqlCatalogScope::Database),
            vec!["connection.read", "schema.describe"],
            false,
            false,
            false,
            Some(30_000),
        ),
        capability(
            "export_query",
            "Export one bounded MySQL query page as CSV, JSON, or JSONL.",
            sql_export_input_schema(SqlInputScope::database(false)),
            sql_export_output_schema(),
            vec!["connection.read", "sql.export", "local.write"],
            false,
            true,
            false,
            Some(60_000),
        ),
        capability(
            "import_plan",
            "Preview one bounded MySQL import batch from an approved local source.",
            sql_import_input_schema(SqlInputScope::database(false)),
            sql_import_output_schema(false),
            vec!["connection.read", "sql.import.plan", "local.read"],
            false,
            false,
            false,
            Some(60_000),
        ),
        capability(
            "import_apply",
            "Apply one acknowledged MySQL import batch from an approved local source.",
            sql_import_input_schema(SqlInputScope::database(false)),
            sql_import_output_schema(true),
            vec!["connection.read", "sql.import.apply", "local.read"],
            true,
            false,
            true,
            Some(60_000),
        ),
    ]
}

pub async fn invoke_mysql_capability(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    if invocation.plugin_id != PLUGIN_ID {
        return Err(validation_error(
            "validation.plugin_mismatch",
            "Invocation plugin_id does not match MySQL.",
            json!({ "expected": PLUGIN_ID, "actual": invocation.plugin_id }),
        ));
    }

    match invocation.capability_id.as_str() {
        "query" => invoke_query(config, invocation).await,
        "explain" => invoke_explain(config, invocation).await,
        "exec" => invoke_exec(config, invocation).await,
        "catalogs" => invoke_catalogs(config, invocation).await,
        "tables" => invoke_tables(config, invocation).await,
        "describe_table" => invoke_describe_table(config, invocation).await,
        "export_query" => invoke_export_query(config, invocation).await,
        "import_plan" => invoke_import(config, invocation, false).await,
        "import_apply" => invoke_import(config, invocation, true).await,
        other => Err(unavailable_error(
            "unavailable.capability_not_found",
            "MySQL capability was not found.",
            json!({ "capability_id": other }),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn capability(
    id: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    permissions: Vec<&str>,
    destructive: bool,
    streaming: bool,
    supports_dry_run: bool,
    default_timeout_ms: Option<u64>,
) -> CapabilityDefinition {
    let (execution_mode, session_handoff) = sql_execution_metadata(id);
    CapabilityDefinition {
        plugin_id: PLUGIN_ID.to_string(),
        id: id.to_string(),
        description: description.to_string(),
        input_schema,
        output_schema,
        permissions: permissions.into_iter().map(str::to_string).collect(),
        authorization: sql_authorization_metadata(id),
        risk: CapabilityRiskLevel::from_destructive(destructive),
        destructive,
        streaming,
        execution_mode,
        session_handoff,
        connection_required: true,
        required_secret_classes: Vec::<CredentialClass>::new(),
        supports_dry_run,
        default_timeout_ms,
    }
}

fn sql_execution_metadata(
    id: &str,
) -> (
    voidb_core::CapabilityExecutionMode,
    Option<voidb_core::CapabilitySessionHandoff>,
) {
    let purpose = match id {
        "query" => voidb_core::PluginSessionPurpose::DatabaseQuery,
        "exec" => voidb_core::PluginSessionPurpose::DatabaseTransaction,
        _ => return (voidb_core::CapabilityExecutionMode::Stateless, None),
    };
    (
        voidb_core::CapabilityExecutionMode::Both,
        Some(voidb_core::CapabilitySessionHandoff::new(
            purpose,
            [format!("{PLUGIN_ID}.{id}")],
        )),
    )
}

fn sql_authorization_metadata(id: &str) -> voidb_core::CapabilityAuthorizationMetadata {
    let metadata = match id {
        "query" => voidb_core::CapabilityAuthorizationMetadata::declared()
            .with_session_purposes(vec![voidb_core::PluginSessionPurpose::DatabaseQuery]),
        "exec" | "import_apply" => voidb_core::CapabilityAuthorizationMetadata::declared()
            .with_interactive_execute()
            .with_session_purposes(vec![voidb_core::PluginSessionPurpose::DatabaseTransaction]),
        _ => voidb_core::CapabilityAuthorizationMetadata::declared(),
    };
    match id {
        "query" | "explain" | "exec" => {
            metadata.with_approval_schema(voidb_core::CapabilityApprovalSchema::v1(vec![
                voidb_core::CapabilityApprovalField::new(
                    "/sql",
                    "SQL statement",
                    voidb_core::CapabilityApprovalValueType::String,
                )
                .required()
                .with_risk_emphasis(if id == "exec" {
                    voidb_core::CapabilityApprovalRiskEmphasis::Destructive
                } else {
                    voidb_core::CapabilityApprovalRiskEmphasis::Normal
                }),
                voidb_core::CapabilityApprovalField::new(
                    "/database",
                    "Database",
                    voidb_core::CapabilityApprovalValueType::ResourceId,
                ),
            ]))
        }
        "tables" => metadata.with_approval_schema(voidb_core::CapabilityApprovalSchema::v1(vec![
            voidb_core::CapabilityApprovalField::new(
                "/database",
                "Database",
                voidb_core::CapabilityApprovalValueType::ResourceId,
            ),
        ])),
        "describe_table" => {
            metadata.with_approval_schema(voidb_core::CapabilityApprovalSchema::v1(vec![
                voidb_core::CapabilityApprovalField::new(
                    "/table",
                    "Table",
                    voidb_core::CapabilityApprovalValueType::ResourceId,
                )
                .required(),
                voidb_core::CapabilityApprovalField::new(
                    "/database",
                    "Database",
                    voidb_core::CapabilityApprovalValueType::ResourceId,
                ),
            ]))
        }
        "export_query" => local_transfer_authorization(metadata, true, false),
        "import_plan" => local_transfer_authorization(metadata, false, false),
        "import_apply" => local_transfer_authorization(metadata, false, true),
        _ => metadata,
    }
}

fn local_transfer_authorization(
    metadata: voidb_core::CapabilityAuthorizationMetadata,
    export: bool,
    destructive: bool,
) -> voidb_core::CapabilityAuthorizationMetadata {
    let mut fields = Vec::new();
    if export {
        fields.push(
            voidb_core::CapabilityApprovalField::new(
                "/sql",
                "SQL statement",
                voidb_core::CapabilityApprovalValueType::String,
            )
            .required(),
        );
    } else {
        fields.push(
            voidb_core::CapabilityApprovalField::new(
                "/table",
                "Target table",
                voidb_core::CapabilityApprovalValueType::ResourceId,
            )
            .required()
            .with_risk_emphasis(if destructive {
                voidb_core::CapabilityApprovalRiskEmphasis::Destructive
            } else {
                voidb_core::CapabilityApprovalRiskEmphasis::Normal
            }),
        );
    }
    fields.extend([
        voidb_core::CapabilityApprovalField::new(
            "/database",
            "Database",
            voidb_core::CapabilityApprovalValueType::ResourceId,
        ),
        voidb_core::CapabilityApprovalField::new(
            "/local_root",
            "Approved local SQL transfer root",
            voidb_core::CapabilityApprovalValueType::Path,
        )
        .with_risk_emphasis(voidb_core::CapabilityApprovalRiskEmphasis::PrivilegeEscalation),
        voidb_core::CapabilityApprovalField::new(
            "/local_path",
            "Relative local SQL transfer path",
            voidb_core::CapabilityApprovalValueType::Path,
        ),
    ]);
    metadata
        .with_approval_schema(voidb_core::CapabilityApprovalSchema::v1(fields))
        .with_note(
            "Local SQL transfer paths are root-relative, no-follow, and revalidated during every read or atomic no-replace write.",
        )
        .without_capability_wide()
}

async fn invoke_query(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let sql = required_string(&invocation.input, "sql")?;
    if !sql_allows_read_only_query(&sql, SqlDialect::MySql) {
        return Err(policy_error(
            "policy.destructive_requires_exec_capability",
            "MySQL query only accepts read-oriented SQL; use mysql.exec for mutating statements.",
            json!({ "capability_id": "query" }),
        ));
    }

    let database = database_for_sql(config, &invocation.input)?;
    let service = service(config).await?;
    let statements = service
        .execute_query(&database, &sql)
        .await
        .map_err(|error| target_error(config, "mysql.query_failed", error.to_string()))?;
    let statements = statements_output(config, statements, page_request(&invocation)?)?;
    Ok(result(
        invocation.id,
        statements.output,
        statements.summary,
        statements.page,
    ))
}

async fn invoke_explain(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let sql = required_string(&invocation.input, "sql")?;
    if optional_bool(&invocation.input, "analyze")?.unwrap_or(false) {
        return Err(policy_error(
            "policy.explain_analyze_disabled",
            "MySQL explain does not run analyze by default because it may execute target work.",
            json!({ "capability_id": "explain", "analyze": true }),
        ));
    }
    if sql_statement_count(&sql, SqlDialect::MySql) != 1 {
        return Err(validation_error(
            "validation.explain_single_statement_required",
            "MySQL explain accepts exactly one SQL statement.",
            json!({ "capability_id": "explain" }),
        ));
    }
    if !sql_allows_read_only_query(&sql, SqlDialect::MySql) {
        return Err(policy_error(
            "policy.destructive_requires_exec_capability",
            "MySQL explain only accepts read-oriented SQL.",
            json!({ "capability_id": "explain" }),
        ));
    }

    let database = database_for_sql(config, &invocation.input)?;
    let explain_sql = sql_explain_statement(&sql, SqlDialect::MySql);
    let service = service(config).await?;
    let statements = service
        .execute_query(&database, &explain_sql)
        .await
        .map_err(|error| target_error(config, "mysql.explain_failed", error.to_string()))?;
    let statements = explain_output(
        statements_output(config, statements, page_request(&invocation)?)?,
        &database,
    );
    Ok(result(
        invocation.id,
        statements.output,
        statements.summary,
        statements.page,
    ))
}

async fn invoke_exec(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let sql = required_string(&invocation.input, "sql")?;
    let database = database_for_sql(config, &invocation.input)?;
    if invocation.controls.dry_run {
        let mut preview = sql_mutation_preview(
            &sql,
            SqlDialect::MySql,
            invocation.controls.acknowledgement.is_some(),
        );
        if let Some(object) = preview.as_object_mut() {
            object.insert("database".to_string(), json!(database));
        }
        return Ok(result(
            invocation.id,
            preview.clone(),
            json!({
                "dry_run": true,
                "operation": "exec",
                "database_selected": !database.is_empty(),
                "statement_count": preview["statement_count"],
                "mutation_gate": preview["mutation_gate"]
            }),
            None,
        ));
    }

    let mutation_gate = sql_mutation_gate(
        &sql,
        SqlDialect::MySql,
        invocation.controls.acknowledgement.is_some(),
        false,
    );
    let service = service(config).await?;
    let statements = service
        .execute_query(&database, &sql)
        .await
        .map_err(|error| target_error(config, "mysql.exec_failed", error.to_string()))?;
    let mut statements = statements_output(config, statements, page_request(&invocation)?)?;
    let rows_affected = statements.rows_affected;
    if let Some(object) = statements.output.as_object_mut() {
        object.insert("rows_affected".to_string(), json!(rows_affected));
        object.insert("mutation_gate".to_string(), mutation_gate.clone());
    }
    if let Some(object) = statements.summary.as_object_mut() {
        object.insert("rows_affected".to_string(), json!(rows_affected));
        object.insert("mutation_gate".to_string(), mutation_gate);
    }
    Ok(result(
        invocation.id,
        statements.output,
        statements.summary,
        statements.page,
    ))
}

async fn invoke_catalogs(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let page = page_request(&invocation)?;
    let (pattern, include_system) = catalog_filters(&invocation.input)?;
    let service = service(config).await?;
    let current_database = config.normalized_database().unwrap_or_default();
    let mut databases = service
        .list_databases()
        .await
        .map_err(|error| target_error(config, "mysql.catalogs_failed", error.to_string()))?
        .into_iter()
        .filter(|database| include_system || !is_mysql_system_database(database))
        .map(|database| {
            sql_catalog_entry(
                &database,
                "database",
                None,
                database == current_database,
                is_mysql_system_database(&database),
            )
        })
        .filter(|database| name_matches(database, pattern.as_deref()))
        .collect::<Vec<_>>();
    let (databases, next_cursor) = page_values(&mut databases, page);
    let output = json!({
        "contract_version": SQL_RESULT_CONTRACT_VERSION,
        "scope": sql_scope(
            SqlCatalogScope::Database,
            config.normalized_database(),
            None
        ),
        "capabilities": sql_introspection_capabilities(mysql_introspection_support()),
        "database": current_database,
        "databases": databases,
        "schemas": [],
        "next_cursor": next_cursor,
        "warnings": [{
            "code": "mysql.schema_is_database",
            "message": "MySQL treats schemas as databases; use the databases array."
        }],
    });
    Ok(result(
        invocation.id,
        output.clone(),
        json!({
            "database_count": output["databases"].as_array().map_or(0, Vec::len),
            "schema_count": 0,
            "next_cursor": output["next_cursor"]
        }),
        output["next_cursor"]
            .as_str()
            .map(|cursor| InvocationOutputPage {
                next_cursor: Some(cursor.to_string()),
            }),
    ))
}

async fn invoke_tables(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let database = required_database(config, &invocation.input)?;
    let page = page_request(&invocation)?;
    let (pattern, include_system) = catalog_filters(&invocation.input)?;
    let service = service(config).await?;
    let (tables, views) = service
        .list_tables(&database)
        .await
        .map_err(|error| target_error(config, "mysql.tables_failed", error.to_string()))?;
    let mut relations = tables
        .iter()
        .map(|table| {
            sql_relation(
                &table.name,
                "table",
                &table.table_type,
                Some(&database),
                None,
                table.rows,
                table.comment.as_deref(),
            )
        })
        .chain(
            views
                .iter()
                .map(|view| sql_relation(view, "view", "view", Some(&database), None, None, None)),
        )
        .filter(|relation| {
            (include_system || !is_mysql_system_relation(relation))
                && name_matches(relation, pattern.as_deref())
        })
        .collect::<Vec<_>>();
    let (relations, next_cursor) = page_values(&mut relations, page);
    let table_entries = relations
        .iter()
        .filter(|relation| relation["relation_type"] == "table")
        .map(|relation| {
            json!({
                "name": relation["name"],
                "table_type": relation["table_type"],
                "rows": relation["rows"],
                "comment": relation["comment"],
            })
        })
        .collect::<Vec<_>>();
    let view_entries = relations
        .iter()
        .filter(|relation| relation["relation_type"] == "view")
        .filter_map(|relation| relation["name"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let output = json!({
        "contract_version": SQL_RESULT_CONTRACT_VERSION,
        "scope": sql_scope(SqlCatalogScope::Database, Some(database.as_str()), None),
        "capabilities": sql_introspection_capabilities(mysql_introspection_support()),
        "database": database.clone(),
        "tables": table_entries,
        "views": view_entries,
        "relations": relations,
        "next_cursor": next_cursor,
        "warnings": [],
    });
    let table_count = output["tables"].as_array().map_or(0, Vec::len);
    let view_count = output["views"].as_array().map_or(0, Vec::len);
    Ok(result(
        invocation.id,
        output.clone(),
        json!({
            "database": database,
            "table_count": table_count,
            "view_count": view_count,
            "next_cursor": output["next_cursor"]
        }),
        output["next_cursor"]
            .as_str()
            .map(|cursor| InvocationOutputPage {
                next_cursor: Some(cursor.to_string()),
            }),
    ))
}

async fn invoke_describe_table(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let database = required_database(config, &invocation.input)?;
    let table = required_string(&invocation.input, "table")?;
    let service = service(config).await?;
    let (columns, indexes, foreign_keys) = service
        .describe_table(&database, &table)
        .await
        .map_err(|error| target_error(config, "mysql.describe_table_failed", error.to_string()))?;
    let constraints = sql_foreign_key_constraints(&foreign_keys);
    let output = json!({
        "scope": sql_scope(SqlCatalogScope::Database, Some(database.as_str()), None),
        "capabilities": sql_introspection_capabilities(mysql_introspection_support()),
        "database": database,
        "table": table,
        "contract_version": SQL_RESULT_CONTRACT_VERSION,
        "columns": columns.iter().enumerate().map(|(index, column)| {
            sql_column_metadata(column, index + 1, SqlDialect::MySql)
        }).collect::<Vec<_>>(),
        "indexes": indexes.into_iter().map(|index| {
            json!({
                "name": index.name,
                "columns": index.columns,
                "unique": index.unique,
                "index_type": index.index_type
            })
        }).collect::<Vec<_>>(),
        "constraints": constraints,
        "foreign_keys": foreign_keys,
        "warnings": [],
    });
    Ok(result(
        invocation.id,
        output.clone(),
        json!({
            "database": output["database"],
            "table": output["table"],
            "column_count": output["columns"].as_array().map_or(0, Vec::len),
            "index_count": output["indexes"].as_array().map_or(0, Vec::len),
            "foreign_key_count": output["foreign_keys"].as_array().map_or(0, Vec::len)
        }),
        None,
    ))
}

async fn invoke_export_query(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let format = sql_data_format(&invocation.input)?;
    let include_header = optional_bool(&invocation.input, "include_header")?.unwrap_or(true);
    let destination = local_destination(&invocation.input)?;
    let database = database_for_sql(config, &invocation.input)?;
    let page = page_request(&invocation)?;
    let sql = sql_export_page_query(
        &required_string(&invocation.input, "sql")?,
        SqlDialect::MySql,
        page.cursor,
        page.limit,
    )
    .map_err(|reason| {
        validation_error(
            "validation.sql_export_query_invalid",
            "MySQL export query is invalid.",
            json!({ "reason": reason }),
        )
    })?;
    let mut query_invocation = invocation.clone();
    query_invocation.capability_id = "query".to_string();
    query_invocation.input = json!({
        "sql": sql,
        "database": database,
    });
    query_invocation.controls.page = Some(Pagination {
        limit: page.limit as u32,
        cursor: None,
    });
    let query = invoke_query(config, query_invocation).await?;
    let chunk = encode_sql_export(&query.output, format, include_header).map_err(|error| {
        validation_error(
            "validation.sql_export_invalid",
            "MySQL export could not encode the bounded query page.",
            json!({ "reason": error }),
        )
    })?;
    enforce_inline_export_limit(&invocation, destination.is_none(), chunk.bytes.len())?;
    let truncated = query.output["truncated"].as_bool().unwrap_or(false);
    let next_cursor = truncated.then(|| page.cursor.saturating_add(chunk.rows.len()).to_string());
    let result_page = next_cursor.as_ref().map(|cursor| InvocationOutputPage {
        next_cursor: Some(cursor.clone()),
    });
    let (destination_written, local_scope_id, preview) =
        write_export_destination(&invocation.id, destination, &chunk.bytes)?;
    let output = json!({
        "format": format.as_str(),
        "columns": chunk.columns,
        "row_count": chunk.rows.len(),
        "bytes": chunk.bytes.len(),
        "truncated": truncated,
        "cursor": if page.cursor == 0 { Value::Null } else { json!(page.cursor.to_string()) },
        "next_cursor": next_cursor,
        "destination_written": destination_written,
        "local_scope_id": local_scope_id,
        "preview": preview,
        "progress": {
            "rows_completed": chunk.rows.len(),
            "bytes_completed": chunk.bytes.len(),
            "terminal": !truncated
        }
    });
    Ok(result(
        invocation.id,
        output.clone(),
        json!({
            "format": output["format"],
            "row_count": output["row_count"],
            "bytes": output["bytes"],
            "truncated": output["truncated"],
            "next_cursor": output["next_cursor"],
            "destination_written": output["destination_written"]
        }),
        result_page,
    ))
}

async fn invoke_import(
    config: &MySqlConfig,
    invocation: CapabilityInvocation,
    apply: bool,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    validate_import_policy(&invocation.input)?;
    let database = required_database(config, &invocation.input)?;
    let table = required_string(&invocation.input, "table")?;
    let format = sql_data_format(&invocation.input)?;
    let page = page_request(&invocation)?;
    let local_root = required_string(&invocation.input, "local_root")?;
    let local_path = required_string(&invocation.input, "local_path")?;
    let local_scope_id = format!("local-scope:{}", invocation.id);
    let local_scope = LocalPathScope::new(&local_root)
        .map_err(|error| local_path_error(&local_scope_id, "read_file", error))?;
    let file = local_scope
        .open_existing_file(&local_path)
        .map_err(|error| local_path_error(&local_scope_id, "read_file", error))?;
    let source_file_bytes = file.len();
    let requested_columns = optional_string_array(&invocation.input, "columns")?;
    let import = parse_sql_import_page(
        file,
        source_file_bytes,
        format,
        optional_bool(&invocation.input, "has_header")?.unwrap_or(true),
        requested_columns.as_deref(),
        page.cursor,
        page.limit,
    )
    .map_err(|error| {
        validation_error(
            "validation.sql_import_invalid",
            "MySQL import source is invalid.",
            json!({ "reason": error }),
        )
    })?;
    let next_cursor = import.next_cursor();
    let dry_run = !apply || invocation.controls.dry_run;
    let rows_inserted = if apply && !invocation.controls.dry_run && !import.rows.is_empty() {
        let sql = build_sql_import_batch(
            SqlDialect::MySql,
            Some(&database),
            None,
            &table,
            &import.columns,
            &import.rows,
        )
        .map_err(|error| {
            validation_error(
                "validation.sql_import_mapping_invalid",
                "MySQL import mapping is invalid.",
                json!({ "reason": error }),
            )
        })?;
        let service = service(config).await?;
        let statements = service
            .execute_query(&database, &sql)
            .await
            .map_err(|error| target_error(config, "mysql.import_failed", error.to_string()))?;
        statements_output(
            config,
            statements,
            PageRequest {
                limit: 1,
                cursor: 0,
            },
        )?;
        import.rows.len()
    } else {
        0
    };
    let mut output = json!({
        "dry_run": dry_run,
        "would_execute": !import.rows.is_empty(),
        "destructive": true,
        "operation": if apply { "import_apply" } else { "import_plan" },
        "format": format.as_str(),
        "table": table,
        "columns": import.columns,
        "row_count": import.rows.len(),
        "source_file_bytes": source_file_bytes,
        "truncated": import.truncated,
        "cursor": if page.cursor == 0 { Value::Null } else { json!(page.cursor.to_string()) },
        "next_cursor": next_cursor,
        "transaction_scope": "batch",
        "row_error_policy": "abort",
        "local_scope_id": local_scope_id,
        "checks": [
            { "code": "local_scope_revalidated", "status": "passed" },
            { "code": "batch_bounded", "status": "passed", "limit": page.limit },
            { "code": "transaction_per_batch", "status": "passed" }
        ],
        "warnings": []
    });
    if apply {
        output["rows_inserted"] = json!(rows_inserted);
    }
    Ok(result(
        invocation.id,
        output.clone(),
        json!({
            "operation": output["operation"],
            "row_count": output["row_count"],
            "rows_inserted": rows_inserted,
            "truncated": output["truncated"],
            "next_cursor": output["next_cursor"],
            "dry_run": dry_run
        }),
        output["next_cursor"]
            .as_str()
            .map(|cursor| InvocationOutputPage {
                next_cursor: Some(cursor.to_string()),
            }),
    ))
}

fn mysql_introspection_support() -> SqlIntrospectionSupport {
    SqlIntrospectionSupport {
        supports_databases: true,
        supports_schemas: false,
        supports_views: true,
        supports_row_counts: true,
        supports_indexes: true,
        supports_foreign_keys: true,
        supports_constraints: true,
        supports_comments: true,
    }
}

async fn service(config: &MySqlConfig) -> Result<MySqlService, CapabilityError> {
    MySqlService::new_direct_capability(config).await
}

#[derive(Debug, Clone, Copy)]
struct PageRequest {
    limit: usize,
    cursor: usize,
}

struct StatementsOutput {
    output: Value,
    summary: Value,
    page: Option<InvocationOutputPage>,
    rows_affected: u64,
}

fn sql_data_format(input: &Value) -> Result<SqlDataFormat, CapabilityError> {
    let format = optional_string(input, "format")?.unwrap_or_else(|| "jsonl".to_string());
    SqlDataFormat::parse(&format).map_err(|reason| {
        validation_error(
            "validation.sql_transfer_format_invalid",
            "SQL transfer format is invalid.",
            json!({ "reason": reason }),
        )
    })
}

fn local_destination(input: &Value) -> Result<Option<(String, String)>, CapabilityError> {
    let root = optional_string(input, "local_root")?;
    let path = optional_string(input, "local_path")?;
    match (root, path) {
        (None, None) => Ok(None),
        (Some(root), Some(path)) => Ok(Some((root, path))),
        _ => Err(validation_error(
            "validation.local_destination_incomplete",
            "local_root and local_path must be supplied together.",
            json!({ "fields": ["local_root", "local_path"] }),
        )),
    }
}

fn write_export_destination(
    invocation_id: &str,
    destination: Option<(String, String)>,
    bytes: &[u8],
) -> Result<(bool, Value, Value), CapabilityError> {
    let Some((root, path)) = destination else {
        return Ok((
            false,
            Value::Null,
            Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ));
    };
    let scope_id = format!("local-scope:{invocation_id}");
    let scope = LocalPathScope::new(root)
        .map_err(|error| local_path_error(&scope_id, "write_new_file", error))?;
    scope
        .write_new_file(path, bytes)
        .map_err(|error| local_path_error(&scope_id, "write_new_file", error))?;
    Ok((true, json!(scope_id), Value::Null))
}

fn enforce_inline_export_limit(
    invocation: &CapabilityInvocation,
    inline: bool,
    bytes: usize,
) -> Result<(), CapabilityError> {
    if inline
        && invocation
            .controls
            .max_output_bytes
            .is_some_and(|maximum| bytes as u64 > maximum)
    {
        return Err(policy_error(
            "policy.output_limit_exceeded",
            "Inline SQL export exceeds max_output_bytes; use a smaller page or an approved local destination.",
            json!({ "bytes": bytes, "maximum": invocation.controls.max_output_bytes }),
        ));
    }
    Ok(())
}

fn validate_import_policy(input: &Value) -> Result<(), CapabilityError> {
    let mode = optional_string(input, "mode")?.unwrap_or_else(|| "append".to_string());
    let row_error_policy =
        optional_string(input, "row_error_policy")?.unwrap_or_else(|| "abort".to_string());
    if mode != "append" || row_error_policy != "abort" {
        return Err(policy_error(
            "policy.sql_import_mode_not_supported",
            "SQL imports currently require append mode and abort-on-error policy.",
            json!({ "mode": mode, "row_error_policy": row_error_policy }),
        ));
    }
    Ok(())
}

fn optional_string_array(
    input: &Value,
    field: &str,
) -> Result<Option<Vec<String>>, CapabilityError> {
    let Some(value) = input.get(field) else {
        return Ok(None);
    };
    let values = value.as_array().ok_or_else(|| {
        validation_error(
            "validation.input_field_invalid",
            "Optional input field must be an array of strings.",
            json!({ "field": field }),
        )
    })?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    validation_error(
                        "validation.input_field_invalid",
                        "Optional input field must contain non-empty strings.",
                        json!({ "field": field }),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn local_path_error(scope_id: &str, access: &str, error: LocalPathError) -> CapabilityError {
    capability_error(
        error.category(),
        error.code(),
        error.safe_message(),
        json!({ "local_scope_id": scope_id, "access": access }),
        None,
        error.retryable(),
        RedactionStatus::Applied,
    )
}

fn catalog_filters(input: &Value) -> Result<(Option<String>, bool), CapabilityError> {
    let pattern = match input.get("pattern") {
        Some(value) if !value.is_string() => {
            return Err(validation_error(
                "validation.input_field_invalid",
                "Catalog pattern must be a string.",
                json!({ "field": "pattern" }),
            ));
        }
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase),
        None => None,
    };
    let include_system = optional_bool(input, "include_system")?.unwrap_or(false);
    Ok((pattern, include_system))
}

fn name_matches(value: &Value, pattern: Option<&str>) -> bool {
    let Some(pattern) = pattern else {
        return true;
    };
    value["name"]
        .as_str()
        .is_some_and(|name| name.to_ascii_lowercase().contains(pattern))
}

fn is_mysql_system_database(database: &str) -> bool {
    matches!(
        database.to_ascii_lowercase().as_str(),
        "information_schema" | "mysql" | "performance_schema" | "sys"
    )
}

fn is_mysql_system_relation(value: &Value) -> bool {
    value["database"]
        .as_str()
        .is_some_and(is_mysql_system_database)
}

fn page_values(values: &mut Vec<Value>, page: PageRequest) -> (Vec<Value>, Option<String>) {
    let source_len = values.len();
    let page_values = values
        .drain(..)
        .skip(page.cursor)
        .take(page.limit)
        .collect::<Vec<_>>();
    let next_offset = page.cursor.saturating_add(page_values.len());
    let next_cursor = (next_offset < source_len).then(|| next_offset.to_string());
    (page_values, next_cursor)
}

fn page_request(invocation: &CapabilityInvocation) -> Result<PageRequest, CapabilityError> {
    let Some(page) = invocation.controls.page.as_ref() else {
        return Ok(PageRequest {
            limit: DEFAULT_ROW_LIMIT,
            cursor: 0,
        });
    };

    if page.limit == 0 || page.limit as usize > MAX_ROW_LIMIT {
        return Err(validation_error(
            "validation.page_limit_invalid",
            "MySQL page limit must be between 1 and the maximum row limit.",
            json!({ "limit": page.limit, "maximum": MAX_ROW_LIMIT }),
        ));
    }

    let cursor = page
        .cursor
        .as_deref()
        .map(|cursor| {
            cursor.parse::<usize>().map_err(|_| {
                validation_error(
                    "validation.cursor_invalid",
                    "MySQL query cursor must be an unsigned integer row offset.",
                    json!({ "cursor": cursor }),
                )
            })
        })
        .transpose()?
        .unwrap_or(0);

    Ok(PageRequest {
        limit: page.limit as usize,
        cursor,
    })
}

fn statements_output(
    config: &MySqlConfig,
    statements: Vec<StatementResult>,
    page: PageRequest,
) -> Result<StatementsOutput, CapabilityError> {
    let mut output_statements = Vec::new();
    let mut rows_affected = 0_u64;
    let mut remaining_skip = page.cursor;
    let mut remaining_take = page.limit;
    let mut returned_row_count = 0_usize;
    let mut source_row_count = 0_usize;
    let mut has_more = false;

    for statement in statements {
        match statement {
            StatementResult::Select { columns, rows } => {
                let statement_source_row_count = rows.len();
                source_row_count += statement_source_row_count;
                let skip_for_statement = remaining_skip.min(statement_source_row_count);
                remaining_skip -= skip_for_statement;
                let available_rows = statement_source_row_count.saturating_sub(skip_for_statement);
                let take_for_statement = remaining_take.min(available_rows);
                remaining_take -= take_for_statement;
                returned_row_count += take_for_statement;
                if available_rows > take_for_statement {
                    has_more = true;
                }

                let rows = rows
                    .iter()
                    .skip(skip_for_statement)
                    .take(take_for_statement)
                    .map(|row| {
                        let mut object = serde_json::Map::new();
                        for (index, column) in columns.iter().enumerate() {
                            let value = row
                                .values
                                .get(index)
                                .map(cell_value_to_json)
                                .unwrap_or(Value::Null);
                            object.insert(column.name.clone(), value);
                        }
                        Value::Object(object)
                    })
                    .collect::<Vec<_>>();
                let column_metadata = columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        sql_column_metadata(column, index + 1, SqlDialect::MySql)
                    })
                    .collect::<Vec<_>>();
                output_statements.push(json!({
                    "kind": "select",
                    "columns": column_metadata,
                    "rows": rows,
                    "row_count": take_for_statement,
                    "source_row_count": statement_source_row_count,
                    "truncated": available_rows > take_for_statement
                }));
            }
            StatementResult::Affected(affected) => {
                rows_affected = rows_affected.saturating_add(affected);
                output_statements.push(json!({
                    "kind": "affected",
                    "rows_affected": affected
                }));
            }
            StatementResult::Empty => {
                output_statements.push(json!({ "kind": "empty" }));
            }
            StatementResult::Error(error) => {
                return Err(target_error(config, "mysql.statement_failed", error));
            }
        }
    }

    let next_cursor = has_more.then(|| (page.cursor + returned_row_count).to_string());
    let output = json!({
        "statements": output_statements,
        "row_limit": page.limit,
        "batch_size": page.limit,
        "row_count": returned_row_count,
        "source_row_count": source_row_count,
        "truncated": has_more,
        "cursor": if page.cursor == 0 { Value::Null } else { json!(page.cursor.to_string()) },
        "next_cursor": next_cursor,
        "contract_version": SQL_RESULT_CONTRACT_VERSION,
        "warnings": []
    });
    let summary = json!({
        "statement_count": output["statements"].as_array().map_or(0, Vec::len),
        "row_count": returned_row_count,
        "source_row_count": source_row_count,
        "truncated": has_more,
        "next_cursor": output["next_cursor"],
        "contract_version": SQL_RESULT_CONTRACT_VERSION,
        "batch_size": page.limit
    });
    let page = has_more.then(|| InvocationOutputPage {
        next_cursor: output["next_cursor"].as_str().map(str::to_string),
    });

    Ok(StatementsOutput {
        output,
        summary,
        page,
        rows_affected,
    })
}

fn explain_output(mut statements: StatementsOutput, database: &str) -> StatementsOutput {
    if let Some(object) = statements.output.as_object_mut() {
        object.insert("analyze".to_string(), json!(false));
        object.insert("format".to_string(), json!("rows"));
        object.insert("dialect".to_string(), json!(SqlDialect::MySql.as_str()));
        object.insert("explained_statement_count".to_string(), json!(1));
        if !database.is_empty() {
            object.insert("database".to_string(), json!(database));
        }
    }
    if let Some(object) = statements.summary.as_object_mut() {
        object.insert("analyze".to_string(), json!(false));
        object.insert("format".to_string(), json!("rows"));
        object.insert("dialect".to_string(), json!(SqlDialect::MySql.as_str()));
        object.insert("explained_statement_count".to_string(), json!(1));
        object.insert("database_selected".to_string(), json!(!database.is_empty()));
    }
    statements
}

fn result(
    invocation_id: String,
    output: Value,
    output_summary: Value,
    page: Option<InvocationOutputPage>,
) -> CapabilityInvocationResult {
    CapabilityInvocationResult {
        invocation_id,
        status: InvocationStatus::Succeeded,
        output,
        output_summary,
        page,
    }
}

fn database_for_sql(config: &MySqlConfig, input: &Value) -> Result<String, CapabilityError> {
    optional_string(input, "database").map(|database| {
        database
            .or_else(|| config.normalized_database().map(str::to_string))
            .unwrap_or_default()
    })
}

fn required_database(config: &MySqlConfig, input: &Value) -> Result<String, CapabilityError> {
    let database = database_for_sql(config, input)?;
    if database.is_empty() {
        return Err(validation_error(
            "validation.database_required",
            "MySQL capability requires a database in input or profile config.",
            json!({ "field": "database" }),
        ));
    }
    Ok(database)
}

fn required_string(input: &Value, field: &str) -> Result<String, CapabilityError> {
    match input.get(field) {
        Some(value) if !value.is_string() => Err(validation_error(
            "validation.input_field_invalid",
            "Required input field must be a string.",
            json!({ "field": field }),
        )),
        Some(value) => value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                validation_error(
                    "validation.input_field_required",
                    "Required string input field is missing.",
                    json!({ "field": field }),
                )
            }),
        None => Err(validation_error(
            "validation.input_field_required",
            "Required string input field is missing.",
            json!({ "field": field }),
        )),
    }
}

fn optional_string(input: &Value, field: &str) -> Result<Option<String>, CapabilityError> {
    match input.get(field) {
        Some(value) if !value.is_string() => Err(validation_error(
            "validation.input_field_invalid",
            "Optional input field must be a string.",
            json!({ "field": field }),
        )),
        Some(value) => Ok(value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)),
        None => Ok(None),
    }
}

fn optional_bool(input: &Value, field: &str) -> Result<Option<bool>, CapabilityError> {
    match input.get(field) {
        Some(value) if !value.is_boolean() => Err(validation_error(
            "validation.input_field_invalid",
            "Optional input field must be a boolean.",
            json!({ "field": field }),
        )),
        Some(value) => Ok(value.as_bool()),
        None => Ok(None),
    }
}

fn validation_error(code: &str, message: &str, details: Value) -> CapabilityError {
    capability_error(
        CapabilityErrorCategory::Validation,
        code,
        message,
        details,
        None,
        false,
        RedactionStatus::NotRequired,
    )
}

fn policy_error(code: &str, message: &str, details: Value) -> CapabilityError {
    capability_error(
        CapabilityErrorCategory::Policy,
        code,
        message,
        details,
        None,
        false,
        RedactionStatus::NotRequired,
    )
}

fn unavailable_error(code: &str, message: &str, details: Value) -> CapabilityError {
    capability_error(
        CapabilityErrorCategory::Unavailable,
        code,
        message,
        details,
        None,
        true,
        RedactionStatus::NotRequired,
    )
}

fn target_error(config: &MySqlConfig, code: &str, message: String) -> CapabilityError {
    let (message, redaction) = redact_mysql_diagnostic(config, &message);
    capability_error(
        CapabilityErrorCategory::TargetSystem,
        code,
        "MySQL target operation failed.",
        Value::Null,
        Some(TargetSystemFailure {
            system: Some(PLUGIN_ID.to_string()),
            code: None,
            message: Some(message),
        }),
        false,
        redaction,
    )
}

fn capability_error(
    category: CapabilityErrorCategory,
    code: &str,
    message: &str,
    details: Value,
    target: Option<TargetSystemFailure>,
    retryable: bool,
    redaction: RedactionStatus,
) -> CapabilityError {
    CapabilityError {
        category,
        code: code.to_string(),
        message: message.to_string(),
        details,
        target,
        retryable,
        redaction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use voidb_core::{
        ActorRef, ActorType, CapabilityErrorCategory, ConnectionInstancePurpose,
        ConnectionProfileRef, InstanceReusePolicy, InvocationConnectionTarget, InvocationControls,
        RedactionStatus, validate_sql_capability_contract,
    };

    #[test]
    fn mysql_capabilities_conform_to_shared_sql_contract() {
        validate_sql_capability_contract(PLUGIN_ID, &mysql_capabilities()).unwrap();
    }

    #[test]
    fn exposes_mysql_capability_metadata() {
        let capabilities = mysql_capabilities();

        assert_eq!(capabilities.len(), 9);
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.qualified_id() == "mysql.query")
        );
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.qualified_id() == "mysql.explain"
                    && !capability.destructive)
        );
        assert!(
            capabilities
                .iter()
                .any(|capability| capability.id == "exec" && capability.destructive)
        );
    }

    #[tokio::test]
    async fn query_rejects_mutating_sql_before_connecting() {
        let error = invoke_mysql_capability(
            &config(),
            invocation("query", json!({ "sql": "drop table users" })),
        )
        .await
        .expect_err("policy error");

        assert_eq!(error.category, CapabilityErrorCategory::Policy);
        assert_eq!(error.code, "policy.destructive_requires_exec_capability");
    }

    #[tokio::test]
    async fn explain_rejects_analyze_before_connecting() {
        let error = invoke_mysql_capability(
            &config(),
            invocation("explain", json!({ "sql": "select 1", "analyze": true })),
        )
        .await
        .expect_err("policy error");

        assert_eq!(error.category, CapabilityErrorCategory::Policy);
        assert_eq!(error.code, "policy.explain_analyze_disabled");
        assert!(error.target.is_none());
    }

    #[tokio::test]
    async fn explain_rejects_mutating_sql_before_connecting() {
        let error = invoke_mysql_capability(
            &config(),
            invocation("explain", json!({ "sql": "drop table users" })),
        )
        .await
        .expect_err("policy error");

        assert_eq!(error.category, CapabilityErrorCategory::Policy);
        assert_eq!(error.code, "policy.destructive_requires_exec_capability");
        assert!(error.target.is_none());
    }

    #[tokio::test]
    async fn exec_dry_run_does_not_connect() {
        let mut invocation = invocation("exec", json!({ "sql": "drop table users" }));
        invocation.controls.dry_run = true;

        let result = invoke_mysql_capability(&config(), invocation)
            .await
            .expect("dry run");

        assert_eq!(result.status, InvocationStatus::Succeeded);
        assert_eq!(result.output["dry_run"], true);
        assert_eq!(result.output_summary["operation"], "exec");
    }

    #[tokio::test]
    async fn tables_requires_database_before_connecting() {
        let mut config = config();
        config.database = None;

        let error = invoke_mysql_capability(&config, invocation("tables", json!({})))
            .await
            .expect_err("database validation");

        assert_eq!(error.category, CapabilityErrorCategory::Validation);
        assert_eq!(error.code, "validation.database_required");
    }

    #[tokio::test]
    async fn invocation_plugin_must_match() {
        let error = invoke_mysql_capability(
            &config(),
            CapabilityInvocation {
                plugin_id: "sqlite".into(),
                ..invocation("query", json!({ "sql": "select 1" }))
            },
        )
        .await
        .expect_err("plugin mismatch");

        assert_eq!(error.category, CapabilityErrorCategory::Validation);
    }

    #[test]
    fn target_error_redacts_profile_values() {
        let error = target_error(
            &config(),
            "mysql.statement_failed",
            "failed mysql://app:mysql-secret@db.internal.example:3306/appdb".into(),
        );

        assert_eq!(error.redaction, RedactionStatus::Applied);
        let message = error
            .target
            .as_ref()
            .and_then(|target| target.message.as_ref())
            .expect("target message");
        assert!(!message.contains("mysql-secret"));
        assert!(!message.contains("db.internal.example"));
    }

    fn config() -> MySqlConfig {
        MySqlConfig::new(
            "db.internal.example".into(),
            3306,
            "app".into(),
            "mysql-secret".into(),
        )
        .with_database("appdb".into())
    }

    fn invocation(capability_id: &str, input: Value) -> CapabilityInvocation {
        CapabilityInvocation {
            id: format!("invoke-{}", capability_id),
            plugin_id: PLUGIN_ID.into(),
            capability_id: capability_id.into(),
            connection: InvocationConnectionTarget::FromProfile {
                profile: ConnectionProfileRef::name("mysql-test"),
                purpose: ConnectionInstancePurpose::CapabilityInvocation,
                reuse: InstanceReusePolicy::Never,
                options: Value::Null,
            },
            input,
            controls: InvocationControls::default(),
            actor: Some(ActorRef {
                id: "agent:test".into(),
                actor_type: ActorType::Agent,
            }),
            requested_at: Utc::now(),
        }
    }
}
