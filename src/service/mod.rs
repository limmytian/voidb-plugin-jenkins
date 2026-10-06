//! Jenkins service layer.
//!
//! Follows the `WebDavService` / `EsService` convention:
//!
//! - Channel mode (`new`): background tokio task processes commands; TUI sends
//!   commands via `send()` and polls `poll_event()` (both non-blocking).
//! - Direct mode (`new_direct`): owns a client inline; exposes async methods
//!   for CLI / test use.
//!
//! Author: Limmy

pub mod commands;
pub mod events;

pub use commands::JenkinsCommand;
pub use events::JenkinsEvent;

use std::sync::Arc;

use anyhow::Result;
use reqwest::Client;
use tokio::sync::mpsc;

use crate::config::JenkinsConfig;
use crate::jenkins_client;
use crate::types::{
    Activity, BuildState, ConsoleChunk, JobDetail, JobSummary, PipelineRun, QueueTrackingState,
};
use voidb_core::TabManager;

/// How many recent builds to fetch for a job by default.
const DEFAULT_MAX_BUILDS: usize = 20;

// ---------------------------------------------------------------------------
// ServiceMode
// ---------------------------------------------------------------------------

/// Internal execution mode.
enum ServiceMode {
    Channel {
        cmd_tx: mpsc::UnboundedSender<JenkinsCommand>,
        event_rx: mpsc::UnboundedReceiver<JenkinsEvent>,
        _task: tokio::task::JoinHandle<()>,
    },
    Direct {
        client: Client,
        config: JenkinsConfig,
    },
}

// ---------------------------------------------------------------------------
// JenkinsService
// ---------------------------------------------------------------------------

/// Facade for all Jenkins operations.
///
/// `Send` but NOT `Sync` (because `UnboundedReceiver` is `!Sync`), so the TUI
/// plugin wraps it in `std::sync::Mutex`.
pub struct JenkinsService {
    mode: ServiceMode,
}

impl JenkinsService {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    /// Create a service in Channel mode (TUI).
    pub fn new(
        config: JenkinsConfig,
        tabs: Arc<dyn TabManager>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<JenkinsCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<JenkinsEvent>();

        let task = runtime.spawn(Self::background_task(cmd_rx, event_tx, tabs, config));

        Self {
            mode: ServiceMode::Channel {
                cmd_tx,
                event_rx,
                _task: task,
            },
        }
    }

    /// Create a service in Direct mode (CLI / tests).
    pub fn new_direct(config: &JenkinsConfig) -> Result<Self> {
        let client = jenkins_client::create_client(config)?;
        Ok(Self {
            mode: ServiceMode::Direct {
                client,
                config: config.clone(),
            },
        })
    }

    // -----------------------------------------------------------------------
    // Channel-mode API (TUI)
    // -----------------------------------------------------------------------

    /// Send a command to the background task (no-op in Direct mode).
    pub fn send(&self, cmd: JenkinsCommand) {
        if let ServiceMode::Channel { cmd_tx, .. } = &self.mode {
            let _ = cmd_tx.send(cmd);
        }
    }

    /// Non-blocking event poll.
    pub fn poll_event(&mut self) -> Option<JenkinsEvent> {
        match &mut self.mode {
            ServiceMode::Channel { event_rx, .. } => event_rx.try_recv().ok(),
            ServiceMode::Direct { .. } => None,
        }
    }

    // -----------------------------------------------------------------------
    // Direct-mode API (CLI / tests)
    // -----------------------------------------------------------------------

    /// Ping Jenkins, returning the master node label (Direct mode only).
    pub async fn ping(&self) -> Result<String> {
        match &self.mode {
            ServiceMode::Direct { client, config } => jenkins_client::ping(client, config).await,
            ServiceMode::Channel { .. } => {
                anyhow::bail!("ping() requires Direct mode")
            }
        }
    }

    /// List jobs inside a folder (Direct mode only).
    pub async fn list_jobs(&self, folder: &str) -> Result<Vec<JobSummary>> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::list_jobs(client, config, folder).await
            }
            ServiceMode::Channel { .. } => anyhow::bail!("list_jobs() requires Direct mode"),
        }
    }

    /// Fetch detailed info for a job (Direct mode only).
    pub async fn get_job_detail(&self, job_full_name: &str) -> Result<JobDetail> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::get_job_detail(client, config, job_full_name, DEFAULT_MAX_BUILDS)
                    .await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("get_job_detail() requires Direct mode")
            }
        }
    }

    /// Trigger a build (Direct mode only).
    pub async fn trigger_build(&self, job_full_name: &str) -> Result<Option<String>> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::trigger_build(client, config, job_full_name).await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("trigger_build() requires Direct mode")
            }
        }
    }

    /// Abort a running build (Direct mode only).
    pub async fn abort_build(&self, job_full_name: &str, build_number: u64) -> Result<()> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::abort_build(client, config, job_full_name, build_number).await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("abort_build() requires Direct mode")
            }
        }
    }

    /// Fetch a chunk of console log (Direct mode only).
    pub async fn fetch_console(
        &self,
        job_full_name: &str,
        build_number: u64,
        start: u64,
    ) -> Result<ConsoleChunk> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::fetch_console(client, config, job_full_name, build_number, start)
                    .await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("fetch_console() requires Direct mode")
            }
        }
    }

    /// Fetch a minimal build state for live wait sessions (Direct mode only).
    pub async fn fetch_build_state(
        &self,
        job_full_name: &str,
        build_number: u64,
    ) -> Result<BuildState> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::fetch_build_state(client, config, job_full_name, build_number).await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("fetch_build_state() requires Direct mode")
            }
        }
    }

    /// Fetch one queue item's transition state (Direct mode only).
    pub async fn fetch_queue_tracking_state(&self, queue_id: i64) -> Result<QueueTrackingState> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::fetch_queue_tracking_state(client, config, queue_id).await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("fetch_queue_tracking_state() requires Direct mode")
            }
        }
    }

    /// Fetch Pipeline stages for a build (Direct mode only).
    pub async fn get_pipeline_run(
        &self,
        job_full_name: &str,
        build_number: u64,
    ) -> Result<PipelineRun> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::get_pipeline_run(client, config, job_full_name, build_number).await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("get_pipeline_run() requires Direct mode")
            }
        }
    }

    /// Fetch currently running and queued builds (Direct mode only).
    pub async fn get_activity(&self) -> Result<Activity> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::get_activity(client, config).await
            }
            ServiceMode::Channel { .. } => anyhow::bail!("get_activity() requires Direct mode"),
        }
    }

    /// Cancel a queued build item (Direct mode only).
    pub async fn cancel_queue_item(&self, queue_id: i64) -> Result<()> {
        match &self.mode {
            ServiceMode::Direct { client, config } => {
                jenkins_client::cancel_queue_item(client, config, queue_id).await
            }
            ServiceMode::Channel { .. } => {
                anyhow::bail!("cancel_queue_item() requires Direct mode")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Background task (Channel mode)
    // -----------------------------------------------------------------------

    async fn background_task(
        mut cmd_rx: mpsc::UnboundedReceiver<JenkinsCommand>,
        event_tx: mpsc::UnboundedSender<JenkinsEvent>,
        tabs: Arc<dyn TabManager>,
        config: JenkinsConfig,
    ) {
        let client = match jenkins_client::create_client(&config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "jenkins: failed to create HTTP client");
                let _ = event_tx.send(JenkinsEvent::Error(format!(
                    "failed to create HTTP client: {}",
                    e
                )));
                let _ = tabs.request_render();
                return;
            }
        };

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                JenkinsCommand::Ping => {
                    tracing::debug!("jenkins: pinging server");
                    match jenkins_client::ping(&client, &config).await {
                        Ok(node_name) => {
                            let _ = event_tx.send(JenkinsEvent::Connected { node_name });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: ping failed");
                            let _ =
                                event_tx.send(JenkinsEvent::Error(format!("ping failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::ListJobs { folder } => {
                    tracing::debug!(folder = %folder, "jenkins: listing jobs");
                    match jenkins_client::list_jobs(&client, &config, &folder).await {
                        Ok(jobs) => {
                            let _ = event_tx.send(JenkinsEvent::JobsListed { folder, jobs });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: list jobs failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("list jobs failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::GetJobDetail { job_full_name } => {
                    tracing::debug!(job = %job_full_name, "jenkins: fetching job detail");
                    match jenkins_client::get_job_detail(
                        &client,
                        &config,
                        &job_full_name,
                        DEFAULT_MAX_BUILDS,
                    )
                    .await
                    {
                        Ok(detail) => {
                            let _ = event_tx.send(JenkinsEvent::JobDetailLoaded { detail });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: get job detail failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("get job detail failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::TriggerBuild { job_full_name } => {
                    tracing::info!(job = %job_full_name, "jenkins: triggering build");
                    match jenkins_client::trigger_build(&client, &config, &job_full_name).await {
                        Ok(queue_url) => {
                            let _ = event_tx.send(JenkinsEvent::BuildTriggered {
                                job_full_name: job_full_name.clone(),
                                queue_url,
                            });
                            // Auto-refresh job detail shortly after triggering.
                            if let Ok(detail) = jenkins_client::get_job_detail(
                                &client,
                                &config,
                                &job_full_name,
                                DEFAULT_MAX_BUILDS,
                            )
                            .await
                            {
                                let _ = event_tx.send(JenkinsEvent::JobDetailLoaded { detail });
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: trigger build failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("trigger build failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::AbortBuild {
                    job_full_name,
                    build_number,
                } => {
                    tracing::info!(
                        job = %job_full_name,
                        build = build_number,
                        "jenkins: aborting build"
                    );
                    match jenkins_client::abort_build(
                        &client,
                        &config,
                        &job_full_name,
                        build_number,
                    )
                    .await
                    {
                        Ok(()) => {
                            let _ = event_tx.send(JenkinsEvent::BuildAborted {
                                job_full_name: job_full_name.clone(),
                                build_number,
                            });
                            if let Ok(detail) = jenkins_client::get_job_detail(
                                &client,
                                &config,
                                &job_full_name,
                                DEFAULT_MAX_BUILDS,
                            )
                            .await
                            {
                                let _ = event_tx.send(JenkinsEvent::JobDetailLoaded { detail });
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: abort build failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("abort build failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::FetchConsole {
                    job_full_name,
                    build_number,
                    start,
                } => {
                    tracing::debug!(
                        job = %job_full_name,
                        build = build_number,
                        start = start,
                        "jenkins: fetching console"
                    );
                    match jenkins_client::fetch_console(
                        &client,
                        &config,
                        &job_full_name,
                        build_number,
                        start,
                    )
                    .await
                    {
                        Ok(chunk) => {
                            let _ = event_tx.send(JenkinsEvent::ConsoleFetched {
                                job_full_name,
                                build_number,
                                chunk,
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: fetch console failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("fetch console failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::GetPipelineRun {
                    job_full_name,
                    build_number,
                } => {
                    tracing::debug!(
                        job = %job_full_name,
                        build = build_number,
                        "jenkins: fetching pipeline overview"
                    );
                    match jenkins_client::get_pipeline_run(
                        &client,
                        &config,
                        &job_full_name,
                        build_number,
                    )
                    .await
                    {
                        Ok(run) => {
                            let _ = event_tx.send(JenkinsEvent::PipelineLoaded { run });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: get pipeline failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("get pipeline failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::GetActivity => {
                    tracing::debug!("jenkins: fetching activity snapshot");
                    match jenkins_client::get_activity(&client, &config).await {
                        Ok(activity) => {
                            let _ = event_tx.send(JenkinsEvent::ActivityLoaded { activity });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: get activity failed");
                            let _ = event_tx
                                .send(JenkinsEvent::Error(format!("get activity failed: {}", e)));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::CancelQueueItem { queue_id } => {
                    tracing::info!(queue_id = queue_id, "jenkins: cancelling queue item");
                    match jenkins_client::cancel_queue_item(&client, &config, queue_id).await {
                        Ok(()) => {
                            let _ = event_tx.send(JenkinsEvent::QueueItemCancelled { queue_id });
                            // Refresh the activity immediately so the user sees it disappear.
                            if let Ok(activity) =
                                jenkins_client::get_activity(&client, &config).await
                            {
                                let _ = event_tx.send(JenkinsEvent::ActivityLoaded { activity });
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "jenkins: cancel queue failed");
                            let _ = event_tx.send(JenkinsEvent::Error(format!(
                                "cancel queue item failed: {}",
                                e
                            )));
                        }
                    }
                    let _ = tabs.request_render();
                }

                JenkinsCommand::Disconnect => {
                    tracing::info!("jenkins: disconnect requested");
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn service_is_send() {
        assert_send::<JenkinsService>();
    }

    #[test]
    fn command_is_send() {
        assert_send::<JenkinsCommand>();
    }

    #[test]
    fn event_is_send() {
        assert_send::<JenkinsEvent>();
    }

    #[test]
    fn mutex_service_is_send_sync() {
        assert_send_sync::<std::sync::Mutex<JenkinsService>>();
    }
}
