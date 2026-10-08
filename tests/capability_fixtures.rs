use chrono::Utc;
use serde_json::{Value, json};
use voidb_core::{
    ActorRef, ActorType, CapabilityErrorCategory, CapabilityInvocation, ConnectionInstancePurpose,
    ConnectionProfileRef, InstanceReusePolicy, InvocationConnectionTarget, InvocationControls,
    InvocationStatus, Pagination, RedactionStatus, validate_sql_capability_contract,
};
use voidb_plugin_mysql::{MySqlConfig, invoke_mysql_capability, mysql_capabilities};

#[test]
fn mysql_contract_fixture_covers_discovery_schema() {
    let capabilities = mysql_capabilities();

    validate_sql_capability_contract("mysql", &capabilities).unwrap();
    assert!(
        capabilities
            .iter()
            .any(|capability| capability.qualified_id() == "mysql.describe_table")
    );
}

#[tokio::test]
async fn mysql_policy_fixture_rejects_mutating_query_without_target() {
    let error = invoke_mysql_capability(
        &fixture_config(),
        invocation("query", json!({ "sql": "delete from users" })),
    )
    .await
    .expect_err("query should reject destructive SQL before connecting");

    assert_eq!(error.category, CapabilityErrorCategory::Policy);
    assert_eq!(error.code, "policy.destructive_requires_exec_capability");
}

#[tokio::test]
async fn mysql_dry_run_fixture_never_requires_live_target() {
    let mut invocation = invocation("exec", json!({ "sql": "create table users(id int)" }));
    invocation.controls.dry_run = true;

    let result = invoke_mysql_capability(&fixture_config(), invocation)
        .await
        .expect("dry-run exec should not connect");

    assert_eq!(result.status, InvocationStatus::Succeeded);
    assert_eq!(result.output["dry_run"], true);
    assert_eq!(result.output["would_execute"], true);
}

#[tokio::test]
async fn mysql_validation_fixture_requires_database_for_metadata() {
    let mut config = fixture_config();
    config.database = None;

    let error = invoke_mysql_capability(
        &config,
        invocation("describe_table", json!({ "table": "users" })),
    )
    .await
    .expect_err("metadata should require selected database before connecting");

    assert_eq!(error.category, CapabilityErrorCategory::Validation);
    assert_eq!(error.code, "validation.database_required");
}

#[tokio::test]
async fn mysql_redaction_fixture_cleans_unavailable_target_error() {
    let mut config = fixture_config();
    config.host = "127.0.0.1".into();
    config.port = 1;

    let error = invoke_mysql_capability(&config, invocation("query", json!({ "sql": "select 1" })))
        .await
        .expect_err("reserved local port should be unavailable");
    let encoded = serde_json::to_string(&error).unwrap();

    assert!(!encoded.contains("mysql-secret"));
    assert_eq!(error.redaction, RedactionStatus::NotRequired);
}

#[tokio::test]
#[ignore = "requires a live MySQL target configured through VOIDB_MYSQL_TEST_* environment variables"]
async fn live_mysql_query_metadata_smoke() {
    let config = live_config_from_env();

    let query = invoke_mysql_capability(
        &config,
        invocation("query", json!({ "sql": "select 1 as value" })),
    )
    .await
    .expect("live query");
    assert_eq!(query.output["statements"][0]["rows"][0]["value"], 1);

    let tables = invoke_mysql_capability(&config, invocation("tables", json!({})))
        .await
        .expect("live tables");
    assert_eq!(tables.status, InvocationStatus::Succeeded);
}

fn fixture_config() -> MySqlConfig {
    MySqlConfig::new(
        "db.internal.example".into(),
        3306,
        "app".into(),
        "mysql-secret".into(),
    )
    .with_database("appdb".into())
}

fn live_config_from_env() -> MySqlConfig {
    let host = std::env::var("VOIDB_MYSQL_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("VOIDB_MYSQL_TEST_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3306);
    let username = std::env::var("VOIDB_MYSQL_TEST_USER").unwrap_or_else(|_| "root".into());
    let password = std::env::var("VOIDB_MYSQL_TEST_PASSWORD").unwrap_or_default();
    let database = std::env::var("VOIDB_MYSQL_TEST_DATABASE").unwrap_or_else(|_| "mysql".into());

    MySqlConfig::new(host, port, username, password).with_database(database)
}

fn invocation(capability_id: &str, input: Value) -> CapabilityInvocation {
    CapabilityInvocation {
        id: format!("fixture-{}", capability_id),
        plugin_id: "mysql".into(),
        capability_id: capability_id.into(),
        connection: InvocationConnectionTarget::FromProfile {
            profile: ConnectionProfileRef::name("mysql-fixture"),
            purpose: ConnectionInstancePurpose::CapabilityInvocation,
            reuse: InstanceReusePolicy::Never,
            options: Value::Null,
        },
        input,
        controls: InvocationControls {
            page: Some(Pagination {
                limit: 50,
                cursor: None,
            }),
            ..InvocationControls::default()
        },
        actor: Some(ActorRef {
            id: "agent:fixture".into(),
            actor_type: ActorType::Agent,
        }),
        requested_at: Utc::now(),
    }
}
