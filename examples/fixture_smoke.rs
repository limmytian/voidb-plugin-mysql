//! MySQL fixture-backed capability smoke driver.
//!
//! This example is script-facing. It exercises the MySQL plugin capability
//! surface against a disposable local MySQL fixture using only the generated
//! database and seed table from the fixture environment.

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde_json::{Value, json};
use voidb_core::{
    ActorRef, ActorType, AgentSessionBinding, AgentSessionCallRequest, AgentSessionOpenContext,
    AgentSessionOpenRequest, AgentSessionRef, CapabilityError, CapabilityErrorCategory,
    CapabilityInvocation, CapabilityInvocationResult, InvocationAcknowledgement,
    InvocationConnectionTarget, InvocationControls, InvocationStatus, Pagination,
    PluginAgentSession, PluginAgentSessionFactory, PluginSessionPurpose, RedactionStatus,
};
use voidb_plugin_mysql::{MySqlAgentSessionFactory, MySqlConfig, invoke_mysql_capability};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = config_from_env()?;
    let database = required_env("VOIDB_MYSQL_SMOKE_DATABASE")?;
    let table = required_env("VOIDB_MYSQL_SMOKE_TABLE")?;
    ensure!(
        database.starts_with("voidb_fixture_"),
        "refusing MySQL smoke outside a generated fixture database"
    );
    ensure!(
        table == "fixture_accounts",
        "refusing MySQL smoke outside the generated fixture seed table"
    );

    let run_id =
        std::env::var("VOIDB_FIXTURE_RUN_ID").unwrap_or_else(|_| "mysql-fixture-smoke".into());
    let scratch_table = format!("fixture_smoke_exec_{}", mysql_sql_id(&run_id));
    let select_seed = format!("SELECT id, name, balance_cents, active FROM `{table}` ORDER BY id");
    let count_seed = format!("SELECT COUNT(*) AS row_count FROM `{table}`");
    let secret_sample = "voidb-mysql-fixture-dry-run-secret";

    let dry_exec = invoke_checked(
        &unavailable_config(),
        "exec",
        json!({
            "database": database,
            "sql": format!("INSERT INTO `{table}` (name, balance_cents, active) VALUES ('{secret_sample}', 0, FALSE)")
        }),
        true,
        false,
        None,
        "mysql.exec dry-run should not require a live target",
    )
    .await?;
    ensure_succeeded(&dry_exec, "mysql.exec dry-run")?;
    ensure!(
        dry_exec.output["dry_run"] == true,
        "mysql.exec dry-run output"
    );
    ensure_result_excludes(&dry_exec, secret_sample, "mysql.exec dry-run")?;

    ensure_policy_error(
        invoke(
            &config,
            "query",
            json!({ "database": database, "sql": format!("DELETE FROM `{table}` WHERE id = 0") }),
            false,
            false,
            None,
        )
        .await,
        "policy.destructive_requires_exec_capability",
        "mysql.query mutating statement",
    )?;

    ensure_policy_error(
        invoke(
            &config,
            "explain",
            json!({ "database": database, "sql": format!("SELECT * FROM `{table}`"), "analyze": true }),
            false,
            false,
            None,
        )
        .await,
        "policy.explain_analyze_disabled",
        "mysql.explain analyze",
    )?;

    let paged = invoke_checked(
        &config,
        "query",
        json!({ "database": database, "sql": select_seed }),
        false,
        false,
        Some(Pagination {
            limit: 2,
            cursor: None,
        }),
        "mysql.query paged seed table",
    )
    .await?;
    ensure_succeeded(&paged, "mysql.query paged")?;
    ensure!(
        paged.output["row_count"].as_u64() == Some(2)
            && paged.output["truncated"] == true
            && paged.output["next_cursor"].as_str() == Some("2")
            && paged
                .page
                .as_ref()
                .and_then(|page| page.next_cursor.as_deref())
                == Some("2"),
        "mysql.query should honor pagination: {}",
        paged.output
    );
    ensure_first_row_name(&paged.output, "alpha fixture account")?;
    ensure_result_excludes_config(&paged, &config, "mysql.query paged")?;

    let next_page = invoke_checked(
        &config,
        "query",
        json!({ "database": database, "sql": select_seed }),
        false,
        false,
        Some(Pagination {
            limit: 2,
            cursor: Some("2".into()),
        }),
        "mysql.query next seed page",
    )
    .await?;
    ensure_succeeded(&next_page, "mysql.query next page")?;
    ensure!(
        next_page.output["row_count"].as_u64() == Some(1) && next_page.output["truncated"] == false,
        "mysql.query next page should return remaining row: {}",
        next_page.output
    );

    let mut no_database_config = config.clone();
    no_database_config.database = None;
    let selected_database_query = invoke_checked(
        &no_database_config,
        "query",
        json!({ "database": database, "sql": count_seed }),
        false,
        false,
        Some(Pagination {
            limit: 50,
            cursor: None,
        }),
        "mysql.query input-selected database",
    )
    .await?;
    ensure_succeeded(&selected_database_query, "mysql.query selected database")?;
    ensure_count_result(&selected_database_query.output, 3)?;

    let tables = invoke_checked(
        &no_database_config,
        "tables",
        json!({ "database": database }),
        false,
        false,
        None,
        "mysql.tables input-selected database",
    )
    .await?;
    ensure_succeeded(&tables, "mysql.tables")?;
    ensure_table_present(&tables.output, &table)?;
    ensure_view_present(&tables.output, "fixture_active_accounts")?;

    let describe = invoke_checked(
        &no_database_config,
        "describe_table",
        json!({ "database": database, "table": table }),
        false,
        false,
        None,
        "mysql.describe_table seed table",
    )
    .await?;
    ensure_succeeded(&describe, "mysql.describe_table")?;
    for column in ["id", "name", "balance_cents", "active", "created_at"] {
        ensure_column_present(&describe.output, column)?;
    }
    ensure_index_present(&describe.output, "PRIMARY")?;

    let explain = invoke_checked(
        &config,
        "explain",
        json!({ "database": database, "sql": format!("SELECT name FROM `{table}` WHERE active = TRUE") }),
        false,
        false,
        Some(Pagination {
            limit: 20,
            cursor: None,
        }),
        "mysql.explain seed query",
    )
    .await?;
    ensure_succeeded(&explain, "mysql.explain")?;
    ensure!(
        explain.output["dialect"] == "mysql" && explain.output["analyze"] == false,
        "mysql.explain should report mysql dialect without analyze: {}",
        explain.output
    );

    cleanup_scratch_table(&config, &database, &scratch_table).await;
    run_persistent_session_smoke(&config, &run_id).await?;
    let create = invoke_checked(
        &config,
        "exec",
        json!({
            "database": database,
            "sql": format!("CREATE TABLE `{scratch_table}` (id INT PRIMARY KEY AUTO_INCREMENT, note VARCHAR(80) NOT NULL, created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP)")
        }),
        false,
        true,
        None,
        "mysql.exec create scratch table",
    )
    .await?;
    ensure_succeeded(&create, "mysql.exec create scratch table")?;
    ensure_exec_acknowledged(&create, "mysql.exec create scratch table")?;

    let insert = invoke_checked(
        &config,
        "exec",
        json!({
            "database": database,
            "sql": format!("INSERT INTO `{scratch_table}` (note) VALUES ('alpha'), ('beta')")
        }),
        false,
        true,
        None,
        "mysql.exec insert scratch rows",
    )
    .await?;
    ensure_succeeded(&insert, "mysql.exec insert scratch rows")?;
    ensure!(
        insert.output["rows_affected"].as_u64() == Some(2),
        "mysql.exec insert rows"
    );

    let scratch_count = invoke_checked(
        &config,
        "query",
        json!({ "database": database, "sql": format!("SELECT COUNT(*) AS row_count FROM `{scratch_table}`") }),
        false,
        false,
        None,
        "mysql.query scratch row count",
    )
    .await?;
    ensure_succeeded(&scratch_count, "mysql.query scratch row count")?;
    ensure_count_result(&scratch_count.output, 2)?;

    let delete = invoke_checked(
        &config,
        "exec",
        json!({ "database": database, "sql": format!("DELETE FROM `{scratch_table}` WHERE note = 'beta'") }),
        false,
        true,
        None,
        "mysql.exec delete scratch row",
    )
    .await?;
    ensure_succeeded(&delete, "mysql.exec delete scratch row")?;
    ensure!(
        delete.output["rows_affected"].as_u64() == Some(1),
        "mysql.exec delete row"
    );
    cleanup_scratch_table(&config, &database, &scratch_table).await;

    ensure_failed_auth_redacts(&config, &database).await?;
    ensure_missing_database_redacts(&config, &database).await?;
    ensure_unavailable_target_redacts(&config, &database).await?;

    println!("mysql fixture capability smoke passed");
    println!("capabilities: query, explain, exec, tables, describe_table");
    println!("fixture_scope: generated MySQL database and seed table");
    Ok(())
}

async fn run_persistent_session_smoke(config: &MySqlConfig, run_id: &str) -> Result<()> {
    let table = format!("agent_session_{}", mysql_sql_id(run_id));
    let factory = MySqlAgentSessionFactory::new(config.clone());
    let purpose = PluginSessionPurpose::DatabaseTransaction;
    let session = factory
        .open(AgentSessionOpenContext {
            binding: AgentSessionBinding {
                grant_id: "mysql-fixture-grant".into(),
                profile_id: "mysql-fixture-profile".into(),
                plugin_id: "mysql".into(),
                purpose: purpose.clone(),
                allowed_capabilities: vec!["mysql.query".into(), "mysql.exec".into()],
                host_generation: 1,
            },
            request: AgentSessionOpenRequest {
                purpose,
                capabilities: vec!["mysql.query".into(), "mysql.exec".into()],
                lease_seconds: 60,
                concurrency: Default::default(),
                destructive_acknowledged: false,
                input: Value::Null,
            },
            lease_expires_at: Utc::now() + chrono::Duration::seconds(60),
        })
        .await
        .map_err(|error| anyhow::anyhow!("open MySQL agent session: {error}"))?;
    session_call(
        session.as_ref(),
        "mysql.exec",
        &format!(
            "SET @voidb_agent_state='kept'; CREATE TEMPORARY TABLE `{table}` (value INT); START TRANSACTION; INSERT INTO `{table}` VALUES (7)"
        ),
        true,
    )
    .await?;
    let state = session_call(
        session.as_ref(),
        "mysql.query",
        &format!("SELECT @voidb_agent_state, COUNT(*) FROM `{table}`"),
        false,
    )
    .await?;
    ensure!(
        state["statements"][0]["rows"][0][0] == "kept"
            && unsigned_value(&state["statements"][0]["rows"][0][1]) == Some(1),
        "MySQL session did not retain variable/temp-table transaction state: {state}"
    );
    session_call(session.as_ref(), "mysql.exec", "ROLLBACK", true).await?;
    let rolled_back = session_call(
        session.as_ref(),
        "mysql.query",
        &format!("SELECT COUNT(*) FROM `{table}`"),
        false,
    )
    .await?;
    ensure!(
        unsigned_value(&rolled_back["statements"][0]["rows"][0][0]) == Some(0),
        "MySQL session rollback did not clear the open transaction: {rolled_back}"
    );
    session.close("fixture close".into()).await?;
    Ok(())
}

async fn session_call(
    session: &dyn PluginAgentSession,
    capability: &str,
    sql: &str,
    acknowledged: bool,
) -> Result<Value> {
    session
        .call(AgentSessionCallRequest {
            session: AgentSessionRef::new("mysql-fixture-session", 1),
            call_id: format!("mysql-session-{capability}"),
            capability: capability.into(),
            input: json!({ "sql": sql, "max_rows": 100 }),
            destructive_acknowledged: acknowledged,
            timeout_ms: Some(10_000),
            output_limit_bytes: 128 * 1024,
        })
        .await
        .map(|result| result.output)
        .map_err(|error| anyhow::anyhow!("MySQL agent session call failed: {error}"))
}

fn config_from_env() -> Result<MySqlConfig> {
    let port = required_env("VOIDB_MYSQL_SMOKE_PORT")?
        .parse::<u16>()
        .context("VOIDB_MYSQL_SMOKE_PORT must be a u16")?;
    let mut config = MySqlConfig::new(
        required_env("VOIDB_MYSQL_SMOKE_HOST")?,
        port,
        required_env("VOIDB_MYSQL_SMOKE_USER")?,
        required_env("VOIDB_MYSQL_SMOKE_PASSWORD")?,
    )
    .with_database(required_env("VOIDB_MYSQL_SMOKE_DATABASE")?)
    .with_ssl_mode(required_env("VOIDB_MYSQL_SMOKE_SSL_MODE")?);
    config.connect_timeout_ms = Some(3_000);
    Ok(config)
}

fn unavailable_config() -> MySqlConfig {
    let mut config = MySqlConfig::new(
        "127.0.0.1".into(),
        1,
        "voidb_mysql_unavailable".into(),
        "voidb-mysql-unavailable-secret".into(),
    )
    .with_database("voidb_mysql_unavailable".into())
    .with_ssl_mode("disabled".into());
    config.connect_timeout_ms = Some(500);
    config
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

fn mysql_sql_id(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' {
            output.push('_');
        }
    }
    let output = output.trim_matches('_');
    if output.is_empty() {
        "run".into()
    } else if output.as_bytes()[0].is_ascii_digit() {
        format!("run_{output}")
    } else {
        output.chars().take(32).collect()
    }
}

async fn invoke(
    config: &MySqlConfig,
    capability_id: &str,
    input: Value,
    dry_run: bool,
    acknowledged: bool,
    page: Option<Pagination>,
) -> std::result::Result<CapabilityInvocationResult, CapabilityError> {
    invoke_mysql_capability(
        config,
        CapabilityInvocation {
            id: format!("mysql-fixture-smoke-{capability_id}"),
            plugin_id: "mysql".into(),
            capability_id: capability_id.into(),
            connection: InvocationConnectionTarget::Stateless,
            input,
            controls: InvocationControls {
                dry_run,
                page,
                acknowledgement: acknowledged.then(acknowledgement),
                ..InvocationControls::default()
            },
            actor: Some(actor()),
            requested_at: Utc::now(),
        },
    )
    .await
}

async fn invoke_checked(
    config: &MySqlConfig,
    capability_id: &str,
    input: Value,
    dry_run: bool,
    acknowledged: bool,
    page: Option<Pagination>,
    label: &str,
) -> Result<CapabilityInvocationResult> {
    invoke(config, capability_id, input, dry_run, acknowledged, page)
        .await
        .map_err(|error| {
            let error_json = serde_json::to_string(&error).unwrap_or_else(|_| format!("{error:?}"));
            anyhow::anyhow!("{label}: {error_json}")
        })
}

fn actor() -> ActorRef {
    ActorRef {
        id: "agent:mysql-fixture-smoke".into(),
        actor_type: ActorType::Agent,
    }
}

fn acknowledgement() -> InvocationAcknowledgement {
    InvocationAcknowledgement {
        actor: actor(),
        acknowledged_at: Utc::now(),
        reason: Some("fixture smoke mutation scoped to generated MySQL database".into()),
        approval_id: Some("mysql-fixture-smoke".into()),
    }
}

fn ensure_succeeded(result: &CapabilityInvocationResult, label: &str) -> Result<()> {
    ensure!(
        result.status == InvocationStatus::Succeeded,
        "{label} returned non-success status: {:?}",
        result.status
    );
    Ok(())
}

fn ensure_exec_acknowledged(result: &CapabilityInvocationResult, label: &str) -> Result<()> {
    ensure!(
        result.output["mutation_gate"]["acknowledged"] == true,
        "{label} did not report acknowledgement: {}",
        result.output
    );
    Ok(())
}

fn ensure_result_excludes(
    result: &CapabilityInvocationResult,
    sample: &str,
    label: &str,
) -> Result<()> {
    let output = serde_json::to_string(&result.output)?;
    let summary = serde_json::to_string(&result.output_summary)?;
    ensure!(
        !output.contains(sample) && !summary.contains(sample),
        "{label} output exposed protected sample"
    );
    Ok(())
}

fn ensure_result_excludes_config(
    result: &CapabilityInvocationResult,
    config: &MySqlConfig,
    label: &str,
) -> Result<()> {
    let output = serde_json::to_string(&result.output)?;
    let summary = serde_json::to_string(&result.output_summary)?;
    for sample in protected_samples(config) {
        ensure!(
            !output.contains(&sample) && !summary.contains(&sample),
            "{label} output exposed MySQL config material"
        );
    }
    Ok(())
}

fn protected_samples(config: &MySqlConfig) -> Vec<String> {
    vec![config.password.clone()]
        .into_iter()
        .filter(|sample| !sample.is_empty())
        .collect()
}

fn ensure_policy_error(
    result: std::result::Result<CapabilityInvocationResult, CapabilityError>,
    code: &str,
    label: &str,
) -> Result<()> {
    match result {
        Ok(result) => bail!("{label}: expected policy error, got {}", result.output),
        Err(error) => {
            ensure!(
                error.category == CapabilityErrorCategory::Policy,
                "{label}: expected policy error, got {:?}",
                error.category
            );
            ensure!(
                error.code == code,
                "{label}: expected {code}, got {}",
                error.code
            );
        }
    }
    Ok(())
}

fn ensure_first_row_name(output: &Value, expected: &str) -> Result<()> {
    let rows = statement_rows(output)?;
    ensure!(
        rows.first().and_then(|row| row["name"].as_str()) == Some(expected),
        "mysql.query first row should be {expected}: {}",
        output
    );
    Ok(())
}

fn ensure_count_result(output: &Value, expected: u64) -> Result<()> {
    let rows = statement_rows(output)?;
    ensure!(
        rows.first()
            .and_then(|row| unsigned_value(&row["row_count"]))
            == Some(expected),
        "mysql.query count result should be {expected}: {}",
        output
    );
    Ok(())
}

fn unsigned_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
}

fn statement_rows(output: &Value) -> Result<&Vec<Value>> {
    output["statements"][0]["rows"]
        .as_array()
        .context("mysql.query output should include first statement rows")
}

fn ensure_table_present(output: &Value, table: &str) -> Result<()> {
    let tables = output["tables"]
        .as_array()
        .context("mysql.tables output should include tables array")?;
    ensure!(
        tables.iter().any(|item| item["name"] == table),
        "mysql.tables did not include expected table: {}",
        output
    );
    Ok(())
}

fn ensure_view_present(output: &Value, view: &str) -> Result<()> {
    let views = output["views"]
        .as_array()
        .context("mysql.tables output should include views array")?;
    ensure!(
        views.iter().any(|item| item.as_str() == Some(view)),
        "mysql.tables did not include expected view: {}",
        output
    );
    Ok(())
}

fn ensure_column_present(output: &Value, column: &str) -> Result<()> {
    let columns = output["columns"]
        .as_array()
        .context("mysql.describe_table output should include columns array")?;
    ensure!(
        columns.iter().any(|item| item["name"] == column),
        "mysql.describe_table did not include expected column {column}: {}",
        output
    );
    Ok(())
}

fn ensure_index_present(output: &Value, index: &str) -> Result<()> {
    let indexes = output["indexes"]
        .as_array()
        .context("mysql.describe_table output should include indexes array")?;
    ensure!(
        indexes.iter().any(|item| item["name"] == index),
        "mysql.describe_table did not include expected index {index}: {}",
        output
    );
    Ok(())
}

async fn ensure_failed_auth_redacts(config: &MySqlConfig, database: &str) -> Result<()> {
    let mut bad = config.clone();
    bad.password.push_str("-wrong-secret");
    match invoke(
        &bad,
        "query",
        json!({ "database": database, "sql": "SELECT 1 AS value" }),
        false,
        false,
        None,
    )
    .await
    {
        Ok(result) => bail!(
            "expected mysql.query auth failure, got output: {}",
            result.output
        ),
        Err(error) => {
            ensure!(
                error.category == CapabilityErrorCategory::Auth,
                "mysql.query bad auth should return auth error, got {:?}",
                error.category
            );
            ensure_error_excludes_samples(
                &error,
                &[config.password.as_str(), bad.password.as_str()],
                "mysql.query bad auth",
            )?;
        }
    }
    Ok(())
}

async fn ensure_missing_database_redacts(config: &MySqlConfig, database: &str) -> Result<()> {
    let missing_database = format!("{database}_missing");
    let mut bad = config.clone();
    bad.database = Some(missing_database.clone());
    match invoke(
        &bad,
        "query",
        json!({ "sql": "SELECT 1 AS value" }),
        false,
        false,
        None,
    )
    .await
    {
        Ok(result) => bail!(
            "expected mysql.query missing database failure, got output: {}",
            result.output
        ),
        Err(error) => {
            ensure!(
                error.code == "target.mysql_database_not_found"
                    || error.code == "auth.mysql_access_denied",
                "mysql.query missing database should return database-not-found or access-denied, got {}",
                error.code
            );
            ensure_error_excludes_samples(
                &error,
                &[config.password.as_str(), missing_database.as_str()],
                "mysql.query missing database",
            )?;
        }
    }
    Ok(())
}

async fn ensure_unavailable_target_redacts(config: &MySqlConfig, database: &str) -> Result<()> {
    let mut unavailable = config.clone();
    unavailable.port = 1;
    match invoke(
        &unavailable,
        "query",
        json!({ "database": database, "sql": "SELECT 1 AS value" }),
        false,
        false,
        None,
    )
    .await
    {
        Ok(result) => bail!(
            "expected mysql.query unavailable target failure, got output: {}",
            result.output
        ),
        Err(error) => {
            ensure!(
                matches!(
                    error.category,
                    CapabilityErrorCategory::Transport
                        | CapabilityErrorCategory::Timeout
                        | CapabilityErrorCategory::TargetSystem
                        | CapabilityErrorCategory::Unavailable
                ),
                "mysql.query unavailable target returned unexpected category {:?}",
                error.category
            );
            ensure_error_excludes_samples(
                &error,
                &[config.password.as_str()],
                "mysql.query unavailable target",
            )?;
        }
    }
    Ok(())
}

fn ensure_error_excludes_samples(
    error: &CapabilityError,
    samples: &[&str],
    label: &str,
) -> Result<()> {
    let text = serde_json::to_string(error)?;
    for sample in samples.iter().copied().filter(|sample| sample.len() >= 4) {
        ensure!(
            !text.contains(sample),
            "{label} error exposed protected sample: {text}"
        );
    }
    ensure!(
        matches!(
            error.redaction,
            RedactionStatus::Applied | RedactionStatus::NotRequired
        ),
        "{label} error reported failed redaction: {:?}",
        error.redaction
    );
    Ok(())
}

async fn cleanup_scratch_table(config: &MySqlConfig, database: &str, table: &str) {
    let _ = invoke(
        config,
        "exec",
        json!({ "database": database, "sql": format!("DROP TABLE IF EXISTS `{table}`") }),
        false,
        true,
        None,
    )
    .await;
}
