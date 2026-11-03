use std::fs;

use async_trait::async_trait;
use chrono::Utc;
use clap::{Arg, ArgAction, ArgMatches, Command};
use voidb_core::VoidbError;
use voidb_core::database::types::QueryResult;
use voidb_core::formatters::{cell_value_to_csv, cell_value_to_json, print_table};
use voidb_core::plugin::cli::{CliContext, CliPlugin};
use voidb_core::{ConnectionProfileRef, TuiLaunchRequest, build_tui_launch_plan};

use crate::config::MySqlConfig;
use crate::service::{MySqlService, StatementResult};
use crate::tui::{
    MySqlTuiLaunch, MySqlTuiSource, build_mysql_tui_evidence, run_mysql_tui,
    write_mysql_tui_preflight,
};

pub struct MySqlCliPlugin;

pub fn create_mysql_cli_plugin() -> Box<dyn CliPlugin> {
    Box::new(MySqlCliPlugin)
}

#[async_trait]
impl CliPlugin for MySqlCliPlugin {
    fn plugin_id(&self) -> &str {
        "mysql"
    }

    fn name(&self) -> &str {
        "MySQL"
    }

    fn commands(&self) -> Vec<Command> {
        let conn_arg = Arg::new("connection")
            .short('c')
            .long("connection")
            .required(true)
            .help("Connection name");

        vec![
            Command::new("query")
                .about("Execute a SQL query")
                .arg(conn_arg.clone())
                .arg(
                    Arg::new("database")
                        .short('d')
                        .long("database")
                        .help("Database name"),
                )
                .arg(
                    Arg::new("format")
                        .short('f')
                        .long("format")
                        .value_parser(["table", "json", "csv"])
                        .default_value("table")
                        .help("Output format"),
                )
                .arg(Arg::new("sql").required(true).help("SQL query to execute")),
            Command::new("databases")
                .about("List databases")
                .arg(conn_arg.clone()),
            Command::new("tables")
                .about("List tables in a database")
                .arg(conn_arg.clone())
                .arg(Arg::new("database").required(true).help("Database name")),
            Command::new("describe")
                .about("Describe table schema")
                .arg(conn_arg.clone())
                .arg(Arg::new("database").required(true).help("Database name"))
                .arg(Arg::new("table").required(true).help("Table name")),
            Command::new("test")
                .about("Test connection to MySQL database")
                .arg(conn_arg.required(false))
                .arg(
                    Arg::new("stdin-config")
                        .long("stdin-config")
                        .action(clap::ArgAction::SetTrue)
                        .help("Read connection configuration JSON from stdin"),
                ),
            Command::new("tui")
                .about("Launch the standalone MySQL data-inspector TUI (pilot)")
                .arg(
                    Arg::new("profile")
                        .long("profile")
                        .value_name("PROFILE")
                        .conflicts_with("connection")
                        .help("Profile name, id:<id>, or name:<name>"),
                )
                .arg(
                    Arg::new("connection")
                        .short('c')
                        .long("connection")
                        .value_name("CONNECTION")
                        .conflicts_with("profile")
                        .help("Legacy connection name"),
                )
                .arg(
                    Arg::new("fixture").long("fixture").value_name("PATH").help(
                        "Load deterministic MySQL TUI fixture JSON instead of opening a server",
                    ),
                )
                .arg(
                    Arg::new("purpose")
                        .long("purpose")
                        .value_name("PURPOSE")
                        .default_value("operations")
                        .help("Launch purpose, for example operations or triage"),
                )
                .arg(
                    Arg::new("readonly")
                        .long("readonly")
                        .action(ArgAction::SetTrue)
                        .help("Stage operation plans but block mutating service commands"),
                )
                .arg(
                    Arg::new("no-restore")
                        .long("no-restore")
                        .action(ArgAction::SetTrue)
                        .help("Start without restoring plugin-owned UI state"),
                )
                .arg(
                    Arg::new("database")
                        .short('d')
                        .long("database")
                        .value_name("DATABASE")
                        .help("Default database (overrides the profile config)"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_parser(["json"])
                        .help("Emit secret-free preflight JSON and exit"),
                )
                .arg(
                    Arg::new("evidence")
                        .long("evidence")
                        .value_name("PATH")
                        .help("Write fixture-backed standalone MySQL TUI evidence JSON and exit"),
                ),
        ]
    }

    async fn execute(
        &self,
        command: &str,
        matches: &ArgMatches,
        ctx: &CliContext,
    ) -> Result<(), VoidbError> {
        match command {
            "query" => self.handle_query(matches, ctx).await,
            "databases" => self.handle_databases(matches, ctx).await,
            "tables" => self.handle_tables(matches, ctx).await,
            "describe" => self.handle_describe(matches, ctx).await,
            "test" => self.handle_test(matches, ctx).await,
            "tui" => self.handle_tui(matches, ctx).await,
            _ => Err(VoidbError::Plugin(format!("Unknown command: {}", command))),
        }
    }
}

impl MySqlCliPlugin {
    fn parse_config(
        conn_name: &str,
        database_override: Option<String>,
        ctx: &CliContext,
    ) -> Result<MySqlConfig, VoidbError> {
        let config = if let Some(config) = ctx.find_connection(conn_name) {
            config.clone()
        } else if let Ok((_profile, config)) =
            ctx.resolve_profile_connection(conn_name, Some("mysql"))
        {
            config
        } else {
            return Err(VoidbError::Plugin(format!(
                "Connection or profile '{}' not found",
                conn_name
            )));
        };

        if config.effective_plugin_id() != "mysql" {
            return Err(VoidbError::Plugin(format!(
                "Connection '{}' is not a MySQL connection (plugin: {})",
                conn_name,
                config.effective_plugin_id()
            )));
        }

        let mut mysql_config: MySqlConfig = serde_json::from_value(
            config
                .plugin_config
                .as_ref()
                .ok_or_else(|| VoidbError::Config("Missing plugin_config".into()))?
                .clone(),
        )
        .map_err(|e| VoidbError::Config(e.to_string()))?;

        if let Some(database) = database_override {
            mysql_config.database = Some(database);
        }

        Ok(mysql_config)
    }

    fn parse_tui_launch(
        matches: &ArgMatches,
        ctx: &CliContext,
    ) -> Result<MySqlTuiLaunch, VoidbError> {
        let fixture_path = matches.get_one::<String>("fixture").cloned();
        let purpose = matches
            .get_one::<String>("purpose")
            .cloned()
            .unwrap_or_else(|| "operations".to_string());
        let readonly = matches.get_flag("readonly");
        let restore = !matches.get_flag("no-restore");
        let database_override = matches
            .get_one::<String>("database")
            .filter(|database| !database.trim().is_empty())
            .cloned();

        if let Some(profile_ref) = matches.get_one::<String>("profile") {
            let (profile, connection) =
                ctx.resolve_profile_connection(profile_ref, Some("mysql"))?;
            let mut config: MySqlConfig = connection
                .plugin_config
                .as_ref()
                .ok_or_else(|| VoidbError::Connection("Missing plugin_config".to_string()))
                .and_then(|pc| {
                    serde_json::from_value(pc.clone())
                        .map_err(|e| VoidbError::Connection(format!("Invalid MySQL config: {}", e)))
                })?;
            if let Some(database) = database_override {
                config.database = Some(database);
            }
            let profile_arg = profile_ref_arg(profile_ref, &profile.id);
            let request = TuiLaunchRequest::new("mysql", profile_arg, purpose.clone())
                .readonly(readonly)
                .restore(restore)
                .raw_input(false);
            let launch_plan = build_tui_launch_plan(&profile, request, "voidb-cli", Utc::now())?;

            return Ok(MySqlTuiLaunch {
                profile_label: profile.name,
                config: Some(config),
                source: MySqlTuiSource::Profile,
                fixture_path,
                purpose,
                readonly,
                restore,
                launch_plan: Some(launch_plan),
            });
        }

        if let Some(conn_name) = matches.get_one::<String>("connection") {
            let config = Self::parse_config(conn_name, database_override, ctx)?;
            return Ok(MySqlTuiLaunch {
                profile_label: conn_name.clone(),
                config: Some(config),
                source: MySqlTuiSource::Connection,
                fixture_path,
                purpose,
                readonly,
                restore,
                launch_plan: None,
            });
        }

        if fixture_path.is_some() {
            return Ok(MySqlTuiLaunch {
                profile_label: "fixture".to_string(),
                config: None,
                source: MySqlTuiSource::Fixture,
                fixture_path,
                purpose,
                readonly,
                restore,
                launch_plan: None,
            });
        }

        Err(VoidbError::Plugin(
            "mysql tui requires --profile, --connection, or --fixture".to_string(),
        ))
    }

    async fn handle_tui(&self, matches: &ArgMatches, ctx: &CliContext) -> Result<(), VoidbError> {
        let launch = Self::parse_tui_launch(matches, ctx)?;
        if let Some(path) = matches.get_one::<String>("evidence") {
            let evidence = build_mysql_tui_evidence(&launch)
                .map_err(|e| VoidbError::Plugin(format!("MySQL TUI evidence failed: {}", e)))?;
            let rendered = serde_json::to_string_pretty(&evidence).map_err(|e| {
                VoidbError::Plugin(format!("MySQL TUI evidence serialization failed: {}", e))
            })?;
            if let Some(parent) = std::path::Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(|e| {
                    VoidbError::Plugin(format!(
                        "Failed to create MySQL TUI evidence directory '{}': {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
            fs::write(path, rendered).map_err(|e| {
                VoidbError::Plugin(format!(
                    "Failed to write MySQL TUI evidence '{}': {}",
                    path, e
                ))
            })?;
            println!("Wrote MySQL TUI evidence to {path}");
            return Ok(());
        }

        if matches
            .get_one::<String>("format")
            .is_some_and(|format| format == "json")
        {
            write_mysql_tui_preflight(&launch)
                .map_err(|e| VoidbError::Plugin(format!("MySQL TUI preflight failed: {}", e)))?;
            return Ok(());
        }

        run_mysql_tui(launch)
            .await
            .map_err(|e| VoidbError::Plugin(format!("MySQL TUI failed: {}", e)))
    }

    async fn create_service(
        &self,
        conn_name: &str,
        ctx: &CliContext,
    ) -> Result<MySqlService, VoidbError> {
        let config = if let Some(config) = ctx.find_connection(conn_name) {
            config.clone()
        } else if let Ok((_profile, config)) =
            ctx.resolve_profile_connection(conn_name, Some("mysql"))
        {
            config
        } else {
            return Err(VoidbError::Plugin(format!(
                "Connection or profile '{}' not found",
                conn_name
            )));
        };

        if config.effective_plugin_id() != "mysql" {
            return Err(VoidbError::Plugin(format!(
                "Connection '{}' is not a MySQL connection (plugin: {})",
                conn_name,
                config.effective_plugin_id()
            )));
        }

        let mysql_config: MySqlConfig = serde_json::from_value(
            config
                .plugin_config
                .as_ref()
                .ok_or_else(|| VoidbError::Config("Missing plugin_config".into()))?
                .clone(),
        )
        .map_err(|e| VoidbError::Config(e.to_string()))?;

        MySqlService::new_direct(&mysql_config)
            .await
            .map_err(VoidbError::Connection)
    }

    async fn handle_query(&self, matches: &ArgMatches, ctx: &CliContext) -> Result<(), VoidbError> {
        let conn_name = matches.get_one::<String>("connection").unwrap();
        let sql = matches.get_one::<String>("sql").unwrap();
        let database = matches
            .get_one::<String>("database")
            .map(|s| s.as_str())
            .unwrap_or("");
        let format = matches
            .get_one::<String>("format")
            .map(|s| s.as_str())
            .unwrap_or("table");

        let service = self.create_service(conn_name, ctx).await?;
        let results = service
            .execute_query(database, sql)
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        for stmt_result in &results {
            match stmt_result {
                StatementResult::Select { columns, rows } => {
                    let result = QueryResult {
                        columns: columns.clone(),
                        rows: rows.clone(),
                        execution_time: std::time::Duration::ZERO,
                        rows_affected: None,
                    };

                    match format {
                        "json" => {
                            let json_rows: Vec<serde_json::Value> = result
                                .rows
                                .iter()
                                .map(|row| {
                                    let mut obj = serde_json::Map::new();
                                    for (i, col) in result.columns.iter().enumerate() {
                                        let val = row
                                            .values
                                            .get(i)
                                            .map(cell_value_to_json)
                                            .unwrap_or(serde_json::Value::Null);
                                        obj.insert(col.name.clone(), val);
                                    }
                                    serde_json::Value::Object(obj)
                                })
                                .collect();
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&json_rows)
                                    .map_err(|e| VoidbError::Plugin(e.to_string()))?
                            );
                        }
                        "csv" => {
                            let header: Vec<&str> =
                                result.columns.iter().map(|c| c.name.as_str()).collect();
                            println!("{}", header.join(","));
                            for row in &result.rows {
                                let values: Vec<String> =
                                    row.values.iter().map(cell_value_to_csv).collect();
                                println!("{}", values.join(","));
                            }
                        }
                        _ => {
                            print_table(&result);
                        }
                    }
                    eprintln!("({} rows)", rows.len());
                }
                StatementResult::Affected(n) => {
                    eprintln!("{} rows affected", n);
                }
                StatementResult::Empty => {
                    eprintln!("(empty result)");
                }
                StatementResult::Error(e) => {
                    return Err(VoidbError::Query(e.clone()));
                }
            }
        }

        Ok(())
    }

    async fn handle_databases(
        &self,
        matches: &ArgMatches,
        ctx: &CliContext,
    ) -> Result<(), VoidbError> {
        let conn_name = matches.get_one::<String>("connection").unwrap();
        let service = self.create_service(conn_name, ctx).await?;
        let databases = service
            .list_databases()
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        for db in databases {
            println!("{}", db);
        }
        Ok(())
    }

    async fn handle_tables(
        &self,
        matches: &ArgMatches,
        ctx: &CliContext,
    ) -> Result<(), VoidbError> {
        let conn_name = matches.get_one::<String>("connection").unwrap();
        let database = matches.get_one::<String>("database").unwrap();
        let service = self.create_service(conn_name, ctx).await?;
        let (tables, _views) = service
            .list_tables(database)
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        println!("{:<40} {:<15} {:>10}", "NAME", "TYPE", "ROWS");
        println!("{}", "-".repeat(65));
        for table in tables {
            println!(
                "{:<40} {:<15} {:>10}",
                table.name,
                table.table_type,
                table
                    .rows
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            );
        }
        Ok(())
    }

    async fn handle_describe(
        &self,
        matches: &ArgMatches,
        ctx: &CliContext,
    ) -> Result<(), VoidbError> {
        let conn_name = matches.get_one::<String>("connection").unwrap();
        let database = matches.get_one::<String>("database").unwrap();
        let table = matches.get_one::<String>("table").unwrap();
        let service = self.create_service(conn_name, ctx).await?;
        let (columns, indexes, foreign_keys) = service
            .describe_table(database, table)
            .await
            .map_err(|e| VoidbError::Query(e.to_string()))?;

        println!(
            "{:<30} {:<20} {:<8} {:<5} DEFAULT",
            "COLUMN", "TYPE", "NULL", "KEY"
        );
        println!("{}", "-".repeat(80));
        for col in &columns {
            println!(
                "{:<30} {:<20} {:<8} {:<5} {}",
                col.name,
                col.data_type,
                if col.nullable { "YES" } else { "NO" },
                if col.is_primary_key { "PRI" } else { "" },
                col.default_value.as_deref().unwrap_or(""),
            );
        }

        if !indexes.is_empty() {
            println!("\nIndexes:");
            for idx in &indexes {
                println!(
                    "  {} ({}) [{}]",
                    idx.name,
                    idx.columns.join(", "),
                    if idx.unique {
                        "UNIQUE"
                    } else {
                        &idx.index_type
                    },
                );
            }
        }

        if !foreign_keys.is_empty() {
            println!("\nForeign Keys:");
            for fk in &foreign_keys {
                println!(
                    "  {} ({}) -> {} ({})",
                    fk.name,
                    fk.columns.join(", "),
                    fk.referenced_table,
                    fk.referenced_columns.join(", "),
                );
            }
        }

        Ok(())
    }

    async fn handle_test(&self, matches: &ArgMatches, ctx: &CliContext) -> Result<(), VoidbError> {
        if matches.get_flag("stdin-config") {
            use std::io::Read;
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(|e| VoidbError::Config(format!("Failed to read stdin: {}", e)))?;
            let val: serde_json::Value = serde_json::from_str(&input)
                .map_err(|e| VoidbError::Config(format!("Invalid JSON on stdin: {}", e)))?;
            let plugin_val = val.get("plugin_config").unwrap_or(&val);
            let mysql_config: MySqlConfig = serde_json::from_value(plugin_val.clone())
                .map_err(|e| VoidbError::Config(format!("Invalid MySQL config: {}", e)))?;
            match MySqlService::new_direct(&mysql_config).await {
                Ok(_) => {
                    println!("OK: Connected successfully to MySQL database");
                    Ok(())
                }
                Err(e) => {
                    println!("FAIL: {}", e);
                    Err(VoidbError::Connection(e))
                }
            }
        } else if let Some(conn_name) = matches.get_one::<String>("connection") {
            match self.create_service(conn_name, ctx).await {
                Ok(_) => {
                    println!("OK: Connected successfully to MySQL database");
                    Ok(())
                }
                Err(e) => {
                    println!("FAIL: {}", e);
                    Err(e)
                }
            }
        } else {
            Err(VoidbError::Plugin(
                "Either --connection or --stdin-config is required".into(),
            ))
        }
    }
}

fn profile_ref_arg(input: &str, profile_id: &str) -> ConnectionProfileRef {
    if let Some(id) = input.strip_prefix("id:") {
        ConnectionProfileRef::Id(id.to_string())
    } else if let Some(name) = input
        .strip_prefix("name:")
        .or_else(|| input.strip_prefix("alias:"))
    {
        ConnectionProfileRef::Name(name.to_string())
    } else if input == profile_id {
        ConnectionProfileRef::Id(input.to_string())
    } else {
        ConnectionProfileRef::Name(input.to_string())
    }
}
