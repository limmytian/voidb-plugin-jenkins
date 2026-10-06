//! Standalone entrypoint for voidb-plugin-jenkins.
//!
//! Provides stdio-jsonrpc server for VoidB external process plugin integration,
//! as well as standalone CLI/TUI subcommands.

use anyhow::Result;
use clap::{Parser, Subcommand};
use voidb_core::{CapabilityError, CapabilityErrorCategory, RedactionStatus};
use voidb_plugin_jenkins::{JenkinsConfig, invoke_jenkins_capability};
use voidb_process_plugin_sdk::{CapabilityRouter, serve_stdio};

#[derive(Parser, Debug)]
#[command(name = "voidb-plugin-jenkins")]
#[command(about = "Jenkins process plugin for VoidB", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run as a stdio-jsonrpc server (default when launched by VoidB)
    Serve,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Serve) | None => run_server(),
    }
}

fn run_server() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut router = CapabilityRouter::new("jenkins");

    for cap in voidb_plugin_jenkins::jenkins_capabilities() {
        let rt = runtime.handle().clone();

        router = router.capability(cap.id, move |invocation, _grants| {
            let config: JenkinsConfig = match &invocation.connection {
                voidb_core::InvocationConnectionTarget::FromProfile { options, .. } => {
                    serde_json::from_value(options.clone()).unwrap_or_default()
                }
                voidb_core::InvocationConnectionTarget::Stateless => JenkinsConfig::default(),
                _ => {
                    return Err(CapabilityError {
                        category: CapabilityErrorCategory::Plugin,
                        code: "jenkins.unsupported_connection_target".into(),
                        message: "Connection target must be FromProfile or Stateless.".into(),
                        details: serde_json::json!({}),
                        target: None,
                        retryable: false,
                        redaction: RedactionStatus::NotRequired,
                    });
                }
            };

            rt.block_on(async {
                invoke_jenkins_capability(&config, invocation).await
            })
        });
    }

    serve_stdio(router)?;
    Ok(())
}

