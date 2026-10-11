//! Standalone entrypoint for voidb-plugin-mysql.
//!
//! Provides stdio-jsonrpc server for VoidB external process plugin integration,
//! as well as standalone CLI execution.

#![allow(clippy::result_large_err)]

use std::io::IsTerminal;
use anyhow::Result;
use voidb_core::config::AppConfig;
use voidb_core::plugin::cli::CliContext;
use voidb_core::{
    CapabilityError, CapabilityErrorCategory, RedactionStatus, VOIDB_MASTER_PASSWORD_ENV,
};
use voidb_plugin_mysql::{
    MySqlConfig, create_mysql_cli_plugin, invoke_mysql_capability, mysql_capabilities,
};
use voidb_process_plugin_sdk::{CapabilityRouter, serve_stdio};

fn main() -> Result<()> {
    let cli_plugin = create_mysql_cli_plugin();
    let mut app = clap::Command::new(env!("CARGO_PKG_NAME"))
        .about("MySQL and MariaDB relational database process plugin for VoidB")
        .version(env!("CARGO_PKG_VERSION"))
        .subcommand(
            clap::Command::new("serve")
                .about("Run as a stdio-jsonrpc server (default when launched by VoidB)"),
        );

    for cmd in cli_plugin.commands() {
        app = app.subcommand(cmd);
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() <= 1 {
        return run_server();
    }

    let matches = app.get_matches();
    match matches.subcommand() {
        Some(("serve", _)) => run_server(),
        Some((cmd, sub_matches)) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let ctx = load_cli_context(sub_matches)?;
                cli_plugin
                    .execute(cmd, sub_matches, &ctx)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))
            })
        }
        None => run_server(),
    }
}

fn load_cli_context(matches: &clap::ArgMatches) -> Result<CliContext> {
    let config_path = match AppConfig::config_path() {
        Ok(path) => path,
        Err(_) => return Ok(CliContext::new(AppConfig::default())),
    };

    if !config_path.exists() {
        return Ok(CliContext::new(AppConfig::default()));
    }

    let raw_config = AppConfig::load_raw_from_path(&config_path)?;
    if !raw_config.requires_master_password() {
        let config = AppConfig::load_from_path_with_password(&config_path, None)?;
        return Ok(CliContext::new(config));
    }

    let is_json = matches.ids().any(|id| id.as_str() == "format")
        && matches.try_get_one::<String>("format").ok().flatten().map(|f| f == "json").unwrap_or(false);
    let has_fixture = matches.ids().any(|id| id.as_str() == "fixture")
        && matches.try_get_one::<String>("fixture").ok().flatten().is_some();
    let has_evidence = matches.ids().any(|id| id.as_str() == "evidence")
        && matches.try_get_one::<String>("evidence").ok().flatten().is_some();
    let has_stdin_config = matches.ids().any(|id| id.as_str() == "stdin-config")
        && matches.get_flag("stdin-config");

    if let Ok(pw) = std::env::var(VOIDB_MASTER_PASSWORD_ENV)
        && !pw.is_empty() {
            let config = AppConfig::load_from_path_with_password(&config_path, Some(&pw))?;
            return Ok(CliContext::new(config).with_master_password(Some(pw)));
        }

    if is_json || has_fixture || has_evidence || has_stdin_config {
        if let Ok(config) = AppConfig::load_from_path(&config_path) {
            return Ok(CliContext::new(config));
        }
        return Ok(CliContext::new(raw_config));
    }

    let has_target = (matches.ids().any(|id| id.as_str() == "connection")
        && matches.try_get_one::<String>("connection").ok().flatten().is_some())
        || (matches.ids().any(|id| id.as_str() == "profile")
        && matches.try_get_one::<String>("profile").ok().flatten().is_some());

    if has_target && std::io::stdin().is_terminal()
        && let Ok(pw) = rpassword::prompt_password("VoidB master password: ")
            && !pw.is_empty() {
                let config = AppConfig::load_from_path_with_password(&config_path, Some(&pw))?;
                return Ok(CliContext::new(config).with_master_password(Some(pw)));
            }

    if let Ok(config) = AppConfig::load_from_path(&config_path) {
        return Ok(CliContext::new(config));
    }
    Ok(CliContext::new(raw_config))
}

fn run_server() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut router = CapabilityRouter::new("mysql");

    for cap in mysql_capabilities() {
        let rt = runtime.handle().clone();

        router = router.capability(cap.id, move |invocation, _grants| {
            let config: MySqlConfig = match &invocation.connection {
                voidb_core::InvocationConnectionTarget::FromProfile { options, .. } => {
                    serde_json::from_value(options.clone()).unwrap_or_default()
                }
                voidb_core::InvocationConnectionTarget::Stateless => MySqlConfig::default(),
                _ => {
                    return Err(CapabilityError {
                        category: CapabilityErrorCategory::Plugin,
                        code: "mysql.unsupported_connection_target".into(),
                        message: "Connection target must be FromProfile or Stateless.".into(),
                        details: serde_json::json!({}),
                        target: None,
                        retryable: false,
                        redaction: RedactionStatus::NotRequired,
                    });
                }
            };

            rt.block_on(async {
                invoke_mysql_capability(&config, invocation).await
            })
        });
    }

    serve_stdio(router)?;
    Ok(())
}
