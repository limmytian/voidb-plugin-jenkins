//! Jenkins connection configuration
//!
//! Author: Limmy

use serde::{Deserialize, Serialize};

fn default_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

/// Jenkins server connection configuration.
///
/// Stored as JSON inside `ConnectionConfig.plugin_config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JenkinsConfig {
    /// Jenkins base URL (e.g. `https://jenkins.example.com`).
    pub url: String,

    /// Authentication method.
    #[serde(default)]
    pub auth: JenkinsAuth,

    /// Connection timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout: u64,

    /// Whether to verify TLS certificates.
    #[serde(default = "default_true")]
    pub verify_ssl: bool,
}

/// Jenkins authentication method.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type")]
pub enum JenkinsAuth {
    /// Anonymous access (no authentication).
    #[default]
    None,
    /// Username + API token (recommended) or username + password.
    Basic { username: String, token: String },
}

impl Default for JenkinsConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            auth: JenkinsAuth::None,
            timeout: 30,
            verify_ssl: true,
        }
    }
}

impl JenkinsConfig {
    /// Return the base URL with trailing slash trimmed.
    pub fn base_url(&self) -> &str {
        self.url.trim_end_matches('/')
    }
}
