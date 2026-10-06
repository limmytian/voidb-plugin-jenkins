//! Jenkins CLI plugin.
//!
//! Jenkins operations stay behind the plugin service layer and capability surface.

use std::fs;

use async_trait::async_trait;
use chrono::Utc;
use clap::{Arg, ArgAction, ArgMatches, Command};
use voidb_core::plugin::cli::{CliContext, CliPlugin};
use voidb_core::{ConnectionProfileRef, TuiLaunchRequest, VoidbError, build_tui_launch_plan};

use crate::config::JenkinsConfig;
use crate::tui::{
    JenkinsTuiLaunch, JenkinsTuiSource, build_jenkins_tui_evidence, run_jenkins_tui,
    write_jenkins_tui_preflight,
};

pub struct JenkinsCliPlugin;

pub fn create_jenkins_cli_plugin() -> Box<dyn CliPlugin> {
    Box::new(JenkinsCliPlugin)
}

#[async_trait]
impl CliPlugin for JenkinsCliPlugin {
    fn plugin_id(&self) -> &str {
        "jenkins"
    }

    fn name(&self) -> &str {
        "Jenkins"
    }

    fn commands(&self) -> Vec<Command> {
        vec![
            Command::new("tui")
                .about("Launch the standalone Jenkins build and console TUI")
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
                .arg(Arg::new("fixture").long("fixture").value_name("PATH").help(
                    "Load deterministic Jenkins TUI fixture JSON instead of opening a server",
                ))
                .arg(
                    Arg::new("purpose")
                        .long("purpose")
                        .value_name("PURPOSE")
                        .default_value("build-triage")
                        .help("Launch purpose, for example build-triage or console"),
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
                    Arg::new("format")
                        .long("format")
                        .value_parser(["json"])
                        .help("Emit secret-free preflight JSON and exit"),
                )
                .arg(
                    Arg::new("evidence")
                        .long("evidence")
                        .value_name("PATH")
                        .help("Write fixture-backed standalone Jenkins TUI evidence JSON and exit"),
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
            "tui" => self.handle_tui(matches, ctx).await,
            _ => Err(VoidbError::Plugin(format!(
                "Unknown jenkins command: {}",
                command
            ))),
        }
    }
}

impl JenkinsCliPlugin {
    fn parse_config(conn_name: &str, ctx: &CliContext) -> Result<JenkinsConfig, VoidbError> {
        let config = ctx
            .find_connection(conn_name)
            .ok_or_else(|| VoidbError::Plugin(format!("Connection '{}' not found", conn_name)))?;

        if config.effective_plugin_id() != "jenkins" {
            return Err(VoidbError::Plugin(format!(
                "Connection '{}' is not a Jenkins connection (plugin: {})",
                conn_name,
                config.effective_plugin_id()
            )));
        }

        config
            .plugin_config
            .as_ref()
            .ok_or_else(|| VoidbError::Connection("Missing plugin_config".to_string()))
            .and_then(|pc| {
                serde_json::from_value(pc.clone())
                    .map_err(|e| VoidbError::Connection(format!("Invalid Jenkins config: {}", e)))
            })
    }

    fn parse_tui_launch(
        matches: &ArgMatches,
        ctx: &CliContext,
    ) -> Result<JenkinsTuiLaunch, VoidbError> {
        let fixture_path = matches.get_one::<String>("fixture").cloned();
        let purpose = matches
            .get_one::<String>("purpose")
            .cloned()
            .unwrap_or_else(|| "build-triage".to_string());
        let readonly = matches.get_flag("readonly");
        let restore = !matches.get_flag("no-restore");

        if let Some(profile_ref) = matches.get_one::<String>("profile") {
            let (profile, connection) =
                ctx.resolve_profile_connection(profile_ref, Some("jenkins"))?;
            let config = connection
                .plugin_config
                .as_ref()
                .ok_or_else(|| VoidbError::Connection("Missing plugin_config".to_string()))
                .and_then(|pc| {
                    serde_json::from_value(pc.clone()).map_err(|e| {
                        VoidbError::Connection(format!("Invalid Jenkins config: {}", e))
                    })
                })?;
            let profile_arg = profile_ref_arg(profile_ref, &profile.id);
            let request = TuiLaunchRequest::new("jenkins", profile_arg, purpose.clone())
                .readonly(readonly)
                .restore(restore)
                .raw_input(false);
            let launch_plan = build_tui_launch_plan(&profile, request, "voidb-cli", Utc::now())?;

            return Ok(JenkinsTuiLaunch {
                profile_label: profile.name,
                config: Some(config),
                source: JenkinsTuiSource::Profile,
                fixture_path,
                purpose,
                readonly,
                restore,
                launch_plan: Some(launch_plan),
            });
        }

        if let Some(conn_name) = matches.get_one::<String>("connection") {
            return Ok(JenkinsTuiLaunch {
                profile_label: conn_name.clone(),
                config: Some(Self::parse_config(conn_name, ctx)?),
                source: JenkinsTuiSource::Connection,
                fixture_path,
                purpose,
                readonly,
                restore,
                launch_plan: None,
            });
        }

        if fixture_path.is_some() {
            return Ok(JenkinsTuiLaunch {
                profile_label: "fixture".to_string(),
                config: None,
                source: JenkinsTuiSource::Fixture,
                fixture_path,
                purpose,
                readonly,
                restore,
                launch_plan: None,
            });
        }

        Err(VoidbError::Plugin(
            "jenkins tui requires --profile, --connection, or --fixture".to_string(),
        ))
    }

    async fn handle_tui(&self, matches: &ArgMatches, ctx: &CliContext) -> Result<(), VoidbError> {
        let launch = Self::parse_tui_launch(matches, ctx)?;
        if let Some(path) = matches.get_one::<String>("evidence") {
            let evidence = build_jenkins_tui_evidence(&launch)
                .map_err(|e| VoidbError::Plugin(format!("Jenkins TUI evidence failed: {}", e)))?;
            let rendered = serde_json::to_string_pretty(&evidence).map_err(|e| {
                VoidbError::Plugin(format!("Jenkins TUI evidence serialization failed: {}", e))
            })?;
            if let Some(parent) = std::path::Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(|e| {
                    VoidbError::Plugin(format!(
                        "Failed to create Jenkins TUI evidence directory '{}': {}",
                        parent.display(),
                        e
                    ))
                })?;
            }
            fs::write(path, rendered).map_err(|e| {
                VoidbError::Plugin(format!(
                    "Failed to write Jenkins TUI evidence '{}': {}",
                    path, e
                ))
            })?;
            println!("Wrote Jenkins TUI evidence to {path}");
            return Ok(());
        }

        if matches
            .get_one::<String>("format")
            .is_some_and(|format| format == "json")
        {
            write_jenkins_tui_preflight(&launch)
                .map_err(|e| VoidbError::Plugin(format!("Jenkins TUI preflight failed: {}", e)))?;
            return Ok(());
        }

        run_jenkins_tui(launch)
            .await
            .map_err(|e| VoidbError::Plugin(format!("Jenkins TUI failed: {}", e)))
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
