//! Jenkins plugin for VoidB.
//!
//! Author: Limmy
//!
//! This plugin provides service and capability operations to:
//! - Browse Jenkins jobs (including folders/multibranch projects)
//! - View recent build history for a job
//! - Trigger new builds (with confirmation)
//! - Abort running builds
//! - Stream console output via Jenkins' `progressiveText` endpoint

mod agent_session;
mod capabilities;
mod cli_plugin;
pub mod config;
pub mod jenkins_client;
pub mod service;
mod tui;
pub mod types;

pub use agent_session::JenkinsAgentSessionFactory;
pub use capabilities::{invoke_jenkins_capability, jenkins_capabilities};
pub use cli_plugin::create_jenkins_cli_plugin;
pub use config::JenkinsConfig;
pub use tui::{
    JENKINS_AGENT_CONTEXT_STORE_DIR_ENV, LEGACY_JENKINS_ASSIST_STORE_DIR_ENV,
    jenkins_agent_context_store_root,
};

/// Test a Jenkins connection defined by a `ConnectionConfig`.
///
/// Returns a short description of the server (node name or URL) on success.
pub async fn test_connection(
    conn: &voidb_core::connection::ConnectionConfig,
) -> anyhow::Result<String> {
    let plugin_config = conn
        .plugin_config
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Missing plugin_config"))?;
    let config: JenkinsConfig = serde_json::from_value(plugin_config.clone())?;

    let client = jenkins_client::create_client(&config)?;
    let node = jenkins_client::ping(&client, &config).await?;

    if node.is_empty() {
        Ok(format!("Jenkins is reachable at {}", config.base_url()))
    } else {
        Ok(format!("Jenkins node '{}' at {}", node, config.base_url()))
    }
}
