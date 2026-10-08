//! Persistent one-connection MySQL agent sessions.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use voidb_core::{
    AgentSessionCallRequest, AgentSessionCallResult, AgentSessionOpenContext, PluginAgentSession,
    PluginAgentSessionFactory, PluginSessionError, PluginSessionErrorCode, PluginSessionHealth,
    PluginSessionPurpose, RedactionTarget, SqlDialect, SqlSessionTransactionTracker,
    collect_redaction_targets, redact_text_with_targets, sql_allows_read_only_query,
};

use crate::{
    MySqlConfig,
    service::{PersistentMySqlConnection, StatementResult},
};

pub struct MySqlAgentSessionFactory {
    config: MySqlConfig,
    redaction_targets: Arc<Vec<RedactionTarget>>,
}

impl MySqlAgentSessionFactory {
    pub fn new(config: MySqlConfig) -> Self {
        let redaction_targets = serde_json::to_value(&config)
            .map(|value| collect_redaction_targets(&value))
            .unwrap_or_default();
        Self {
            config,
            redaction_targets: Arc::new(redaction_targets),
        }
    }
}

#[async_trait]
impl PluginAgentSessionFactory for MySqlAgentSessionFactory {
    fn plugin_id(&self) -> &str {
        "mysql"
    }

    async fn open(
        &self,
        context: AgentSessionOpenContext,
    ) -> Result<Arc<dyn PluginAgentSession>, PluginSessionError> {
        validate_open(&context)?;
        let mut connection = PersistentMySqlConnection::open(&self.config)
            .await
            .map_err(|_| {
                error(
                    PluginSessionErrorCode::OwnerUnavailable,
                    "MySQL session connection could not be opened.",
                )
            })?;
        let transaction = SqlSessionTransactionTracker::from_open_input(
            SqlDialect::MySql,
            &context.request.input,
        )
        .map_err(|reason| {
            PluginSessionError::new(
                PluginSessionErrorCode::PolicyDenied,
                format!("MySQL session transaction options are invalid: {reason}"),
            )
        })?;
        if let Some(sql) = transaction.isolation_setup_sql() {
            let results = connection.execute(sql).await.map_err(|_| {
                error(
                    PluginSessionErrorCode::OwnerUnavailable,
                    "MySQL session isolation could not be configured.",
                )
            })?;
            if results_have_error(&results) {
                return Err(error(
                    PluginSessionErrorCode::Unsupported,
                    "MySQL session isolation is unsupported by the target.",
                ));
            }
        }
        Ok(Arc::new(MySqlAgentSession {
            connection: Mutex::new(Some(connection)),
            transaction: Mutex::new(transaction),
            redaction_targets: Arc::clone(&self.redaction_targets),
        }))
    }
}

struct MySqlAgentSession {
    connection: Mutex<Option<PersistentMySqlConnection>>,
    transaction: Mutex<SqlSessionTransactionTracker>,
    redaction_targets: Arc<Vec<RedactionTarget>>,
}

#[async_trait]
impl PluginAgentSession for MySqlAgentSession {
    async fn call(
        &self,
        request: AgentSessionCallRequest,
    ) -> Result<AgentSessionCallResult, PluginSessionError> {
        let sql = validate_call(&request, "mysql", SqlDialect::MySql)?;
        let mut transaction = self.transaction.lock().await;
        let plan = transaction.plan(sql).map_err(|reason| {
            PluginSessionError::new(
                PluginSessionErrorCode::PolicyDenied,
                format!("MySQL transaction transition is invalid: {reason}"),
            )
        })?;
        let mut guard = self.connection.lock().await;
        let connection = guard.as_mut().ok_or_else(|| {
            error(
                PluginSessionErrorCode::OwnerUnavailable,
                "MySQL session is closed.",
            )
        })?;
        let results = connection.execute(sql).await.map_err(|_| {
            error(
                PluginSessionErrorCode::OwnerUnavailable,
                "MySQL session SQL failed.",
            )
        })?;
        if results_have_error(&results) {
            let rollback_succeeded = if transaction.failure_requires_rollback(&plan) {
                connection
                    .execute("ROLLBACK")
                    .await
                    .is_ok_and(|results| !results_have_error(&results))
            } else {
                true
            };
            transaction.record_failure(&plan, rollback_succeeded);
        } else {
            transaction.record_success(plan);
        }
        let transaction_status = transaction.status();
        bounded_results(
            request,
            mysql_results_json(results),
            transaction_status,
            &self.redaction_targets,
        )
    }

    async fn health(&self) -> Result<PluginSessionHealth, PluginSessionError> {
        Ok(if self.connection.lock().await.is_some() {
            PluginSessionHealth::Ready
        } else {
            PluginSessionHealth::Closed
        })
    }
    async fn cancel(&self, _call_id: &str) -> Result<(), PluginSessionError> {
        close_connection(&self.transaction, &self.connection).await;
        Ok(())
    }
    async fn close(&self, _reason: String) -> Result<(), PluginSessionError> {
        close_connection(&self.transaction, &self.connection).await;
        Ok(())
    }
}

async fn close_connection(
    transaction: &Mutex<SqlSessionTransactionTracker>,
    connection: &Mutex<Option<PersistentMySqlConnection>>,
) {
    let _transaction = transaction.lock().await;
    if let Some(connection) = connection.lock().await.take() {
        connection.close().await;
    }
}

fn validate_open(context: &AgentSessionOpenContext) -> Result<(), PluginSessionError> {
    if !matches!(
        context.binding.purpose,
        PluginSessionPurpose::DatabaseQuery | PluginSessionPurpose::DatabaseTransaction
    ) {
        return Err(error(
            PluginSessionErrorCode::Unsupported,
            "MySQL persistent sessions require a database purpose.",
        ));
    }
    if context
        .binding
        .allowed_capabilities
        .iter()
        .any(|capability| {
            !matches!(
                capability.as_str(),
                "mysql.query" | "query" | "mysql.exec" | "exec"
            )
        })
    {
        return Err(error(
            PluginSessionErrorCode::PolicyDenied,
            "MySQL session capability scope is limited to query and exec.",
        ));
    }
    Ok(())
}

fn validate_call<'a>(
    request: &'a AgentSessionCallRequest,
    plugin: &str,
    dialect: SqlDialect,
) -> Result<&'a str, PluginSessionError> {
    let sql = request
        .input
        .get("sql")
        .and_then(Value::as_str)
        .filter(|sql| !sql.trim().is_empty())
        .ok_or_else(|| error(PluginSessionErrorCode::PolicyDenied, "SQL is required."))?;
    let query = format!("{plugin}.query");
    let exec = format!("{plugin}.exec");
    if request.capability == query && !sql_allows_read_only_query(sql, dialect) {
        return Err(error(
            PluginSessionErrorCode::PolicyDenied,
            "Read-oriented session capability rejected mutating SQL.",
        ));
    }
    if request.capability == exec && !request.destructive_acknowledged {
        return Err(error(
            PluginSessionErrorCode::PolicyDenied,
            "Database exec requires destructive acknowledgement.",
        ));
    }
    if request.capability != query && request.capability != exec {
        return Err(error(
            PluginSessionErrorCode::PolicyDenied,
            "Database session accepts query and exec.",
        ));
    }
    Ok(sql)
}

fn mysql_results_json(results: Vec<StatementResult>) -> Vec<Value> {
    results.into_iter().map(|result| match result {
        StatementResult::Select { columns, rows } => json!({ "kind": "rows", "columns": columns.iter().map(|column| column.name.clone()).collect::<Vec<_>>(), "rows": rows.into_iter().map(|row| row.values.into_iter().map(|value| voidb_core::formatters::cell_value_to_json(&value)).collect::<Vec<_>>()).collect::<Vec<_>>() }),
        StatementResult::Affected(rows) => json!({ "kind": "affected", "rows": rows }),
        StatementResult::Empty => json!({ "kind": "empty" }),
        StatementResult::Error(_) => json!({ "kind": "error", "message": "MySQL statement failed; issue ROLLBACK before continuing a failed transaction." }),
    }).collect()
}

fn results_have_error(results: &[StatementResult]) -> bool {
    results
        .iter()
        .any(|result| matches!(result, StatementResult::Error(_)))
}

fn bounded_results(
    request: AgentSessionCallRequest,
    statements: Vec<Value>,
    transaction: Value,
    redaction_targets: &[RedactionTarget],
) -> Result<AgentSessionCallResult, PluginSessionError> {
    let max_rows = request
        .input
        .get("max_rows")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 1000) as usize;
    let statements = statements
        .into_iter()
        .map(|mut statement| {
            if let Some(rows) = statement.get_mut("rows").and_then(Value::as_array_mut) {
                let source_count = rows.len();
                rows.truncate(max_rows);
                statement["source_count"] = json!(source_count);
                statement["truncated"] = json!(source_count > max_rows);
            }
            statement
        })
        .collect::<Vec<_>>();
    let encoded = serde_json::to_string(
        &json!({ "statements": statements, "max_rows": max_rows, "transaction": transaction }),
    )
    .map_err(|_| {
        error(
            PluginSessionErrorCode::OwnerUnavailable,
            "Database session output could not be encoded.",
        )
    })?;
    let (redacted, _) = redact_text_with_targets(&encoded, redaction_targets);
    let output = serde_json::from_str(&redacted)
        .unwrap_or_else(|_| json!({ "statements": [], "redacted": true }));
    AgentSessionCallResult::bounded(request.call_id, output, request.output_limit_bytes)
}

fn error(code: PluginSessionErrorCode, message: &str) -> PluginSessionError {
    PluginSessionError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_errors_and_results_do_not_expose_password() {
        let factory = MySqlAgentSessionFactory::new(MySqlConfig::new(
            "invalid".into(),
            3306,
            "user".into(),
            "super-secret".into(),
        ));
        let request = AgentSessionCallRequest {
            session: voidb_core::AgentSessionRef::new("session", 1),
            call_id: "call".into(),
            capability: "mysql.query".into(),
            input: json!({"sql":"SELECT 'super-secret'", "max_rows": 1}),
            destructive_acknowledged: false,
            timeout_ms: Some(1),
            output_limit_bytes: 4096,
        };
        let output = bounded_results(
            request,
            vec![json!({"rows":[["super-secret"]]})],
            json!({ "state": "idle" }),
            &factory.redaction_targets,
        )
        .unwrap();
        assert!(!output.output.to_string().contains("super-secret"));
    }
}
