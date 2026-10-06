use std::collections::VecDeque;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
#[cfg(test)]
use crossterm::event::KeyModifiers;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use voidb_core::{
    ActorRef, ActorType, AgentContextShare, AgentContextSharePolicy, AgentContextShareStatus,
    AgentContextShareStore, AgentOperation, AgentOperationConfirmation, AgentOperationRequest,
    AgentOperationTarget, AppConfig, AssistBoundedText, AssistContextPolicy, AssistContextSnapshot,
    AssistOwnerLease, AssistPluginState, AssistSessionBinding, AssistWithheldField,
    AssistWithholdingReason, DEFAULT_ASSIST_REQUEST_TTL_SECONDS, PluginSessionHealth,
    PluginSessionPurpose, PluginSessionRegistration, PluginSessionScope, RedactionStatus, TabInfo,
    TabManager, TuiLaunchPlan, retained_tui_quality_gate,
};
#[cfg(test)]
use voidb_core::{AgentOperationRisk, AgentPrincipal, AssistPermission};

use crate::config::{JenkinsAuth, JenkinsConfig};
use crate::service::{JenkinsCommand, JenkinsEvent, JenkinsService};
use crate::types::{
    Activity, BuildSummary, ConsoleChunk, JobDetail, JobSummary, QueueItem, RunningBuild,
};

const CONSOLE_LINE_LIMIT: usize = 5_000;
const CONSOLE_BYTE_LIMIT: usize = 5 * 1024 * 1024;
const CONSOLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const OPERATION_SYNC_INTERVAL: Duration = Duration::from_millis(250);
pub const JENKINS_AGENT_CONTEXT_STORE_DIR_ENV: &str = "VOIDB_JENKINS_AGENT_CONTEXT_DIR";
#[doc(hidden)]
pub const LEGACY_JENKINS_ASSIST_STORE_DIR_ENV: &str = "VOIDB_JENKINS_ASSIST_DIR";

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JenkinsTuiSource {
    Profile,
    Connection,
    Fixture,
}

#[derive(Debug, Clone)]
pub struct JenkinsTuiLaunch {
    pub profile_label: String,
    pub config: Option<JenkinsConfig>,
    pub source: JenkinsTuiSource,
    pub fixture_path: Option<String>,
    pub purpose: String,
    pub readonly: bool,
    pub restore: bool,
    pub launch_plan: Option<TuiLaunchPlan>,
}

#[derive(Debug, Deserialize)]
struct JenkinsTuiFixture {
    profile_label: Option<String>,
    server_label: String,
    auth_method: String,
    #[serde(default)]
    jobs: Vec<FixtureJob>,
    #[serde(default)]
    builds: Vec<FixtureBuild>,
    #[serde(default)]
    running: Vec<FixtureRunningBuild>,
    #[serde(default)]
    queue: Vec<FixtureQueueItem>,
    #[serde(default)]
    console: Vec<String>,
    status: Option<String>,
    server_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureJob {
    name: String,
    full_name: String,
    status: String,
    url: String,
    #[serde(default)]
    folder: bool,
    #[serde(default)]
    buildable: bool,
    #[serde(default)]
    in_queue: bool,
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureBuild {
    job_full_name: String,
    number: u64,
    result: Option<String>,
    #[serde(default)]
    building: bool,
    #[serde(default)]
    duration: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureRunningBuild {
    job_name: String,
    job_full_name: String,
    build_number: u64,
    node: String,
    executor: u32,
    progress: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct FixtureQueueItem {
    id: i64,
    job_name: String,
    job_full_name: String,
    why: String,
    #[serde(default)]
    blocked: bool,
    #[serde(default)]
    stuck: bool,
}

#[derive(Debug, Clone)]
struct JenkinsTuiData {
    profile_label: String,
    server_label: String,
    auth_method: String,
    jobs: Vec<JobView>,
    builds: Vec<BuildView>,
    running: Vec<RunningView>,
    queue: Vec<QueueView>,
    console: Vec<String>,
    status: String,
    server_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JobView {
    name: String,
    full_name: String,
    status: String,
    url: String,
    folder: bool,
    buildable: bool,
    in_queue: bool,
    description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildView {
    job_full_name: String,
    number: u64,
    result: String,
    building: bool,
    duration: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RunningView {
    job_name: String,
    job_full_name: String,
    build_number: u64,
    node: String,
    executor: u32,
    progress: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueView {
    id: i64,
    job_name: String,
    job_full_name: String,
    why: String,
    blocked: bool,
    stuck: bool,
}

impl From<JobSummary> for JobView {
    fn from(job: JobSummary) -> Self {
        let full_name = job.name.clone();
        let status = job.status_label().to_string();
        let folder = job.is_folder();
        Self {
            name: job.name,
            full_name,
            status,
            url: job.url,
            folder,
            buildable: !folder,
            in_queue: false,
            description: String::new(),
        }
    }
}

impl From<FixtureJob> for JobView {
    fn from(job: FixtureJob) -> Self {
        Self {
            name: job.name,
            full_name: job.full_name,
            status: job.status,
            url: job.url,
            folder: job.folder,
            buildable: job.buildable,
            in_queue: job.in_queue,
            description: job.description.unwrap_or_default(),
        }
    }
}

impl From<BuildSummary> for BuildView {
    fn from(build: BuildSummary) -> Self {
        Self {
            job_full_name: String::new(),
            number: build.number,
            result: build.result_label().to_string(),
            building: build.building,
            duration: build.duration,
        }
    }
}

impl From<FixtureBuild> for BuildView {
    fn from(build: FixtureBuild) -> Self {
        Self {
            job_full_name: build.job_full_name,
            number: build.number,
            result: build.result.unwrap_or_else(|| {
                if build.building {
                    "RUNNING".to_string()
                } else {
                    "UNKNOWN".to_string()
                }
            }),
            building: build.building,
            duration: build.duration,
        }
    }
}

impl From<RunningBuild> for RunningView {
    fn from(build: RunningBuild) -> Self {
        Self {
            job_name: build.job_name,
            job_full_name: build.job_full_name.unwrap_or_default(),
            build_number: build.build_number.unwrap_or_default(),
            node: build.node,
            executor: build.executor,
            progress: build.progress,
        }
    }
}

impl From<FixtureRunningBuild> for RunningView {
    fn from(build: FixtureRunningBuild) -> Self {
        Self {
            job_name: build.job_name,
            job_full_name: build.job_full_name,
            build_number: build.build_number,
            node: build.node,
            executor: build.executor,
            progress: build.progress,
        }
    }
}

impl From<QueueItem> for QueueView {
    fn from(item: QueueItem) -> Self {
        Self {
            id: item.id,
            job_name: item.job_name,
            job_full_name: item.job_full_name.unwrap_or_default(),
            why: item.why,
            blocked: item.blocked,
            stuck: item.stuck,
        }
    }
}

impl From<FixtureQueueItem> for QueueView {
    fn from(item: FixtureQueueItem) -> Self {
        Self {
            id: item.id,
            job_name: item.job_name,
            job_full_name: item.job_full_name,
            why: item.why,
            blocked: item.blocked,
            stuck: item.stuck,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewKind {
    Jobs,
    Builds,
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browser,
    Filter,
    Help,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserItem {
    Job(JobView),
    Build(BuildView),
    Running(RunningView),
    Queue(QueueView),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OperationKind {
    TriggerBuild,
    RetryBuild,
    AbortBuild,
    CancelQueue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationPlanView {
    kind: OperationKind,
    job_full_name: String,
    build_number: Option<u64>,
    queue_id: Option<i64>,
    target_label: String,
    risk: &'static str,
    confirmations_required: u8,
    confirmations: u8,
}

#[derive(Debug, Clone)]
struct BoundedLines {
    lines: VecDeque<String>,
    bytes: usize,
    dropped_lines: usize,
    max_lines: usize,
    max_bytes: usize,
}

pub fn write_jenkins_tui_preflight(launch: &JenkinsTuiLaunch) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&preflight_value(launch))?
    );
    Ok(())
}

pub fn build_jenkins_tui_evidence(launch: &JenkinsTuiLaunch) -> Result<Value> {
    let data = if let Some(path) = &launch.fixture_path {
        load_fixture(path)?
    } else {
        let config = launch
            .config
            .as_ref()
            .context("jenkins tui evidence requires a profile, connection, or fixture")?;
        config_data(launch.profile_label.clone(), config)
    };

    let job_target = data
        .jobs
        .iter()
        .find(|job| job.buildable)
        .map(|job| job.full_name.clone())
        .unwrap_or_else(|| "job".to_string());
    let build_target = data
        .running
        .first()
        .map(|build| format!("{} #{}", build.job_full_name, build.build_number))
        .or_else(|| {
            data.builds
                .first()
                .map(|build| format!("{} #{}", build.job_full_name, build.number))
        })
        .unwrap_or_else(|| "build".to_string());
    let mut evidence = json!({
        "schema_version": 1,
        "kind": "jenkins_tui_fixture_evidence",
        "quality_gate": retained_tui_quality_gate(
            "jenkins",
            &["fixture-jenkins-ops", "fixture Jenkins"],
            &["server_error", "401 unauthorized"],
            80,
            24,
            10_000
        ),
        "preflight": preflight_value(launch),
        "transcript": {
            "profile_label": data.profile_label,
            "server_label": data.server_label,
            "auth_method": data.auth_method,
            "jobs": data.jobs.iter().map(job_value).collect::<Vec<_>>(),
            "builds": data.builds.iter().map(build_value).collect::<Vec<_>>(),
            "running": data.running.iter().map(running_value).collect::<Vec<_>>(),
            "queue": data.queue.iter().map(queue_value).collect::<Vec<_>>(),
            "console": {
                "line_count": data.console.len(),
                "lines": data.console,
                "byte_limit": CONSOLE_BYTE_LIMIT
            },
            "server_error": data.server_error
        },
        "plan_transcript": [
            {
                "action": "trigger_build",
                "risk": "side_effecting",
                "confirmation": "explicit_y",
                "parameter_preview": "visible_before_submit"
            },
            {
                "action": "abort_build",
                "risk": "destructive",
                "first_confirmation": "summary_visible",
                "second_confirmation": "required_before_service_command",
                "readonly_behavior": "plan_visible_but_blocked"
            },
            {
                "action": "retry_build",
                "risk": "side_effecting",
                "confirmation": "explicit_y"
            }
        ],
        "coverage": [
            "startup",
            "job_navigator",
            "build_navigator",
            "queue_running_failed_states",
            "console_follow_bounds",
            "console_search_filter",
            "trigger_parameter_preview",
            "stop_double_confirmation",
            "retry_confirmation",
            "readonly_block",
            "context_share_snapshot",
            "external_agent_operation_review",
            "operation_decision_record",
            "current_pty_rejected",
            "server_error",
            "resize",
            "quit_restore",
            "secret_leak_scan"
        ],
        "external_agent_interaction": {
            "store_env": JENKINS_AGENT_CONTEXT_STORE_DIR_ENV,
            "broker_policy": "non_pty",
            "context_share": {
                "active_view": "jobs",
                "job_target": job_target,
                "build_target": build_target,
                "bounded_context": true,
                "withheld_fields": [
                    "jenkins.client_handle",
                    "jenkins.auth",
                    "jenkins.console.secret_lines",
                    "jenkins.profile_config"
                ]
            },
            "operation_review": {
                "capabilities": [
                    "jenkins.trigger_build",
                    "jenkins.abort_build",
                    "jenkins.cancel_queue_item"
                ],
                "current_pty_allowed": false,
                "decision_record": "AgentOperationConfirmation",
                "service_execution_requires_existing_plan_confirmation": true
            }
        },
        "secret_leak_scan": null
    });

    let rendered = serde_json::to_string(&evidence)?;
    let markers = secret_leak_markers(&rendered);
    evidence["secret_leak_scan"] = json!({
        "passed": markers.is_empty(),
        "marker_count": markers.len(),
        "markers": markers
    });
    Ok(evidence)
}

pub async fn run_jenkins_tui(launch: JenkinsTuiLaunch) -> Result<()> {
    let mut app = JenkinsTuiApp::new(launch)?;
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &mut app);
    app.shutdown();
    ratatui::restore();
    result
}

fn preflight_value(launch: &JenkinsTuiLaunch) -> Value {
    let fixture_data = launch
        .fixture_path
        .as_ref()
        .and_then(|path| load_fixture(path).ok());
    let server = launch
        .config
        .as_ref()
        .map(server_value)
        .or_else(|| {
            fixture_data.as_ref().map(|data| {
                json!({
                    "server_label": data.server_label,
                    "auth_method": data.auth_method,
                    "secret_material": "redacted"
                })
            })
        })
        .unwrap_or_else(|| {
            json!({
                "server_label": "fixture",
                "auth_method": "fixture",
                "secret_material": "redacted"
            })
        });
    let launch_plan = launch.launch_plan.as_ref().map(|plan| {
        json!({
            "schema_version": plan.schema_version,
            "plugin_id": plan.plugin_id,
            "command": plan.command,
            "args": plan.args,
            "profile": plan.profile,
            "purpose": plan.purpose,
            "readonly": plan.readonly,
            "restore": plan.restore,
            "raw_input": plan.raw_input,
            "credential_ref_count": plan.credential_grant.credential_refs.len(),
            "credential_grant_id": plan.credential_grant.id,
            "redaction": plan.redaction
        })
    });

    json!({
        "ok": true,
        "command": "jenkins tui",
        "plugin_id": "jenkins",
        "profile_label": fixture_data
            .as_ref()
            .map(|data| data.profile_label.clone())
            .unwrap_or_else(|| launch.profile_label.clone()),
        "source": launch.source,
        "purpose": launch.purpose,
        "readonly": launch.readonly,
        "restore": launch.restore,
        "raw_input": false,
        "fixture": launch.fixture_path.is_some(),
        "server": server,
        "privacy": {
            "diagnostics_include_api_tokens": false,
            "diagnostics_include_crumbs": false,
            "diagnostics_include_cookies": false,
            "diagnostics_include_secret_parameters": false
        },
        "stream_bounds": {
            "console": { "max_lines": CONSOLE_LINE_LIMIT, "max_bytes": CONSOLE_BYTE_LIMIT }
        },
        "service_boundary": "JenkinsService::Channel",
        "modes": [
            "jobs",
            "builds",
            "activity",
            "console_follow",
            "search_filter",
            "operation_plan",
            "server_error"
        ],
        "launch_plan": launch_plan
    })
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut JenkinsTuiApp) -> Result<()> {
    terminal.draw(|frame| app.draw(frame))?;
    loop {
        let mut dirty = app.drain_service();
        app.maybe_poll_console();
        dirty |= app.sync_agent_operation();
        if app.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    app.handle_key(key);
                    dirty = true;
                }
                Event::Resize(cols, rows) => {
                    app.status = format!("resized jenkins view to {cols}x{rows}");
                    dirty = true;
                }
                _ => {}
            }
        }
        if dirty {
            terminal.draw(|frame| app.draw(frame))?;
        }
    }
}

fn load_fixture(path: &str) -> Result<JenkinsTuiData> {
    let text = fs::read_to_string(path).with_context(|| format!("read fixture {path}"))?;
    let fixture: JenkinsTuiFixture =
        serde_json::from_str(&text).with_context(|| format!("parse fixture {path}"))?;
    Ok(JenkinsTuiData {
        profile_label: fixture
            .profile_label
            .unwrap_or_else(|| "fixture-jenkins".to_string()),
        server_label: fixture.server_label,
        auth_method: fixture.auth_method,
        jobs: fixture.jobs.into_iter().map(JobView::from).collect(),
        builds: fixture.builds.into_iter().map(BuildView::from).collect(),
        running: fixture.running.into_iter().map(RunningView::from).collect(),
        queue: fixture.queue.into_iter().map(QueueView::from).collect(),
        console: fixture.console,
        status: fixture
            .status
            .unwrap_or_else(|| "fixture jenkins build triage ready".to_string()),
        server_error: fixture.server_error,
    })
}

fn config_data(profile_label: String, config: &JenkinsConfig) -> JenkinsTuiData {
    JenkinsTuiData {
        profile_label,
        server_label: redacted_url(config.base_url()),
        auth_method: auth_label(&config.auth).to_string(),
        jobs: Vec::new(),
        builds: Vec::new(),
        running: Vec::new(),
        queue: Vec::new(),
        console: Vec::new(),
        status: "connecting through JenkinsService channel mode".to_string(),
        server_error: None,
    }
}

struct JenkinsTuiApp {
    profile_label: String,
    server_label: String,
    auth_method: String,
    source: JenkinsTuiSource,
    purpose: String,
    readonly: bool,
    restore: bool,
    active: ViewKind,
    jobs: Vec<JobView>,
    builds: Vec<BuildView>,
    running: Vec<RunningView>,
    queue: Vec<QueueView>,
    selected: usize,
    filter: String,
    service: Option<JenkinsService>,
    operation_plan: Option<OperationPlanView>,
    status: String,
    mode: Mode,
    return_mode: Mode,
    console: BoundedLines,
    console_follow: Option<(String, u64)>,
    console_offset: u64,
    console_has_more: bool,
    last_console_poll: Instant,
    context_share_store: AgentContextShareStore,
    context_share: Option<AgentContextShare>,
    context_share_owner_lease: Option<AssistOwnerLease>,
    operation_request: Option<AgentOperationRequest>,
    operation_request_seen_id: Option<String>,
    operation_confirmation: Option<AgentOperationConfirmation>,
    context_share_sequence: u64,
    last_operation_sync: Instant,
    should_quit: bool,
    render_quit: Arc<AtomicBool>,
}

impl JenkinsTuiApp {
    fn new(launch: JenkinsTuiLaunch) -> Result<Self> {
        let data = if let Some(path) = &launch.fixture_path {
            load_fixture(path)?
        } else {
            let config = launch
                .config
                .as_ref()
                .context("jenkins tui requires a profile, connection, or fixture")?;
            config_data(launch.profile_label.clone(), config)
        };

        let render_quit = Arc::new(AtomicBool::new(false));
        let service = if launch.fixture_path.is_none() {
            let config = launch
                .config
                .clone()
                .context("jenkins tui requires config outside fixture mode")?;
            let tabs = Arc::new(StandaloneJenkinsTabManager::new(render_quit.clone()));
            let runtime = tokio::runtime::Handle::current();
            let service = JenkinsService::new(config, tabs, runtime);
            service.send(JenkinsCommand::Ping);
            service.send(JenkinsCommand::ListJobs {
                folder: String::new(),
            });
            service.send(JenkinsCommand::GetActivity);
            Some(service)
        } else {
            None
        };

        let mut console = BoundedLines::new(CONSOLE_LINE_LIMIT, CONSOLE_BYTE_LIMIT);
        for line in &data.console {
            console.push_line(line.clone());
        }
        let status = data
            .server_error
            .as_deref()
            .map(|error| format!("{} | {}", data.status, safe_error_summary(error)))
            .unwrap_or_else(|| data.status.clone());

        Ok(Self {
            profile_label: data.profile_label,
            server_label: data.server_label,
            auth_method: data.auth_method,
            source: launch.source,
            purpose: launch.purpose,
            readonly: launch.readonly,
            restore: launch.restore,
            active: ViewKind::Jobs,
            jobs: data.jobs,
            builds: data.builds,
            running: data.running,
            queue: data.queue,
            selected: 0,
            filter: String::new(),
            service,
            operation_plan: None,
            status,
            mode: Mode::Browser,
            return_mode: Mode::Browser,
            console,
            console_follow: None,
            console_offset: 0,
            console_has_more: false,
            last_console_poll: Instant::now(),
            context_share_store: jenkins_context_share_store()?,
            context_share: None,
            context_share_owner_lease: None,
            operation_request: None,
            operation_request_seen_id: None,
            operation_confirmation: None,
            context_share_sequence: 0,
            last_operation_sync: Instant::now() - OPERATION_SYNC_INTERVAL,
            should_quit: false,
            render_quit,
        })
    }

    fn shutdown(&mut self) {
        self.cancel_context_share_on_shutdown();
        if let Some(service) = &self.service {
            service.send(JenkinsCommand::Disconnect);
        }
    }

    fn cancel_context_share_on_shutdown(&mut self) {
        let Some(share_id) = self.context_share.as_ref().map(|share| share.id.clone()) else {
            return;
        };
        let now = Utc::now();
        let _ = self.context_share_store.update_plugin_state(
            &share_id,
            AssistPluginState {
                mode: mode_label(self.mode).to_string(),
                health: PluginSessionHealth::Closed,
                status: "Jenkins TUI owner closed".to_string(),
                updated_at: now,
                metadata: json!({}),
                redaction: RedactionStatus::NotRequired,
            },
        );
        let _ = self.context_share_store.cancel(&share_id);
        self.operation_request = None;
        if self.operation_confirmation.take().is_some() {
            self.operation_plan = None;
        }
        self.context_share_owner_lease = None;
    }

    fn drain_service(&mut self) -> bool {
        let mut changed = false;
        if self.render_quit.load(Ordering::SeqCst) {
            changed |= !self.should_quit;
            self.should_quit = true;
        }
        let Some(mut service) = self.service.take() else {
            return changed;
        };
        while let Some(event) = service.poll_event() {
            changed = true;
            self.handle_service_event(event);
        }
        self.service = Some(service);
        changed
    }

    fn handle_service_event(&mut self, event: JenkinsEvent) {
        match event {
            JenkinsEvent::Connected { node_name } => {
                self.status = if node_name.is_empty() {
                    "connected to Jenkins".to_string()
                } else {
                    format!("connected to Jenkins node {node_name}")
                };
            }
            JenkinsEvent::JobsListed { folder: _, jobs } => {
                self.jobs = jobs.into_iter().map(JobView::from).collect();
                self.jobs.sort_by(|left, right| left.name.cmp(&right.name));
                self.trim_selection();
                self.status = format!("listed {} Jenkins jobs", self.jobs.len());
            }
            JenkinsEvent::JobDetailLoaded { detail } => {
                self.apply_job_detail(detail);
            }
            JenkinsEvent::BuildTriggered {
                job_full_name,
                queue_url,
            } => {
                self.operation_plan = None;
                self.status = format!(
                    "triggered {job_full_name}{}",
                    queue_url
                        .as_ref()
                        .map(|url| format!(" queue={}", redacted_url(url)))
                        .unwrap_or_default()
                );
            }
            JenkinsEvent::BuildAborted {
                job_full_name,
                build_number,
            } => {
                self.operation_plan = None;
                self.status = format!("abort requested for {job_full_name} #{build_number}");
            }
            JenkinsEvent::ConsoleFetched {
                job_full_name,
                build_number,
                chunk,
            } => {
                self.apply_console_chunk(job_full_name, build_number, chunk);
            }
            JenkinsEvent::ActivityLoaded { activity } => {
                self.apply_activity(activity);
            }
            JenkinsEvent::QueueItemCancelled { queue_id } => {
                self.operation_plan = None;
                self.status = format!("queue item {queue_id} cancellation requested");
            }
            JenkinsEvent::PipelineLoaded { run } => {
                self.status = format!(
                    "pipeline {} #{}: {} stages",
                    run.job_full_name,
                    run.build_number,
                    run.stages.len()
                );
            }
            JenkinsEvent::Error(message) => {
                self.mode = Mode::Error;
                self.status = safe_error_summary(&message);
            }
        }
    }

    fn apply_job_detail(&mut self, detail: JobDetail) {
        let full_name = detail.full_name.clone();
        self.builds = detail
            .builds
            .into_iter()
            .map(|build| BuildView {
                job_full_name: full_name.clone(),
                ..BuildView::from(build)
            })
            .collect();
        self.active = ViewKind::Builds;
        self.selected = 0;
        self.status = format!("loaded {} builds for {}", self.builds.len(), full_name);
    }

    fn apply_activity(&mut self, activity: Activity) {
        self.running = activity
            .running
            .into_iter()
            .map(RunningView::from)
            .collect();
        self.queue = activity.queue.into_iter().map(QueueView::from).collect();
        self.status = format!(
            "activity: {} running, {} queued",
            self.running.len(),
            self.queue.len()
        );
    }

    fn apply_console_chunk(
        &mut self,
        job_full_name: String,
        build_number: u64,
        chunk: ConsoleChunk,
    ) {
        self.console.push_text(&redact_console(&chunk.text));
        self.console_offset = chunk.next_offset;
        self.console_has_more = chunk.has_more;
        self.console_follow = Some((job_full_name.clone(), build_number));
        self.status = format!(
            "console {} #{} offset {} has_more={}",
            job_full_name, build_number, self.console_offset, self.console_has_more
        );
    }

    fn maybe_poll_console(&mut self) {
        if self.last_console_poll.elapsed() < CONSOLE_POLL_INTERVAL {
            return;
        }
        self.last_console_poll = Instant::now();
        let Some((job_full_name, build_number)) = self.console_follow.clone() else {
            return;
        };
        if !self.console_has_more && self.console_offset > 0 {
            return;
        }
        if let Some(service) = &self.service {
            service.send(JenkinsCommand::FetchConsole {
                job_full_name,
                build_number,
                start: self.console_offset,
            });
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.mode == Mode::Help {
            self.mode = self.return_mode;
            self.status = format!("returned to {}", mode_label(self.mode));
            return;
        }
        if self.mode == Mode::Error {
            self.mode = Mode::Browser;
            self.status = "returned to Jenkins browser".to_string();
            return;
        }
        if self.mode == Mode::Filter {
            self.handle_filter_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab => self.next_view(),
            KeyCode::BackTab => self.previous_view(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.filtered_items().len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('i') => self.open_selected(),
            KeyCode::Char('/') => {
                self.return_mode = self.mode;
                self.mode = Mode::Filter;
                self.status = "search: type text, Enter apply, Esc clear".to_string();
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('f') => self.follow_console(),
            KeyCode::Char('c') => self.cancel_console(),
            KeyCode::Char('t') => self.plan_trigger(),
            KeyCode::Char('R') => self.plan_retry(),
            KeyCode::Char('x') | KeyCode::Delete => self.plan_stop_or_cancel(),
            KeyCode::Char('a') => self.share_agent_context(),
            KeyCode::Char('y')
                if self.operation_request.is_some() && self.operation_plan.is_none() =>
            {
                self.stage_agent_operation()
            }
            KeyCode::Char('n')
                if self.operation_request.is_some() && self.operation_plan.is_none() =>
            {
                self.deny_agent_operation()
            }
            KeyCode::Char('y') => self.confirm_plan(),
            KeyCode::Esc => self.cancel_plan(),
            KeyCode::Char('?') => self.show_help(),
            _ => {
                self.status =
                    "jenkins: Tab views, j/k move, / search, Enter detail, f console, a share context, y/n review agent operation, t/R/x plan, q quit"
                        .to_string();
            }
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.mode = self.return_mode;
                self.selected = 0;
                self.status = format!("search applied: {}", self.filter_label());
            }
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = self.return_mode;
                self.selected = 0;
                self.status = "search cleared".to_string();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(ch) => {
                self.filter.push(ch);
                self.selected = 0;
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered_items().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let last = len as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    fn next_view(&mut self) {
        self.active = match self.active {
            ViewKind::Jobs => ViewKind::Builds,
            ViewKind::Builds => ViewKind::Activity,
            ViewKind::Activity => ViewKind::Jobs,
        };
        self.selected = 0;
        self.status = format!("view: {}", self.active.label());
    }

    fn previous_view(&mut self) {
        self.active = match self.active {
            ViewKind::Jobs => ViewKind::Activity,
            ViewKind::Builds => ViewKind::Jobs,
            ViewKind::Activity => ViewKind::Builds,
        };
        self.selected = 0;
        self.status = format!("view: {}", self.active.label());
    }

    fn refresh(&mut self) {
        if let Some(service) = &self.service {
            service.send(JenkinsCommand::ListJobs {
                folder: String::new(),
            });
            service.send(JenkinsCommand::GetActivity);
            self.status = "refreshing Jenkins jobs and activity".to_string();
        } else {
            self.status = "fixture refresh: Jenkins jobs and activity".to_string();
        }
    }

    fn open_selected(&mut self) {
        match self.selected_item() {
            Some(BrowserItem::Job(job)) => {
                if job.folder {
                    self.status = format!("folder selected: {}", job.full_name);
                    return;
                }
                if let Some(service) = &self.service {
                    service.send(JenkinsCommand::GetJobDetail {
                        job_full_name: job.full_name.clone(),
                    });
                    self.status = format!("loading builds for {}", job.full_name);
                } else {
                    self.active = ViewKind::Builds;
                    self.selected = 0;
                    self.status = format!("fixture builds for {}", job.full_name);
                }
            }
            Some(BrowserItem::Build(build)) => {
                self.start_console(build.job_full_name, build.number);
            }
            Some(BrowserItem::Running(build)) => {
                self.start_console(build.job_full_name, build.build_number);
            }
            Some(BrowserItem::Queue(item)) => {
                self.status = format!("queue {}: {}", item.id, item.why);
            }
            None => self.status = "nothing selected".to_string(),
        }
    }

    fn follow_console(&mut self) {
        match self.selected_item() {
            Some(BrowserItem::Build(build)) => {
                self.start_console(build.job_full_name, build.number)
            }
            Some(BrowserItem::Running(build)) => {
                self.start_console(build.job_full_name, build.build_number);
            }
            _ => self.status = "select a build or running item for console follow".to_string(),
        }
    }

    fn start_console(&mut self, job_full_name: String, build_number: u64) {
        if job_full_name.is_empty() || build_number == 0 {
            self.status = "console follow requires job and build number".to_string();
            return;
        }
        self.console.clear();
        self.console_offset = 0;
        self.console_has_more = true;
        self.console_follow = Some((job_full_name.clone(), build_number));
        if let Some(service) = &self.service {
            service.send(JenkinsCommand::FetchConsole {
                job_full_name: job_full_name.clone(),
                build_number,
                start: 0,
            });
            self.status = format!("following console for {job_full_name} #{build_number}");
        } else {
            self.status = format!("fixture console for {job_full_name} #{build_number}");
        }
    }

    fn cancel_console(&mut self) {
        self.console_follow = None;
        self.console_has_more = false;
        self.status = "console follow stopped".to_string();
    }

    fn share_agent_context(&mut self) {
        let policy = AssistContextPolicy::default();
        let snapshot = match self.build_context_share_snapshot(&policy) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.status = format!("context snapshot failed: {error}");
                return;
            }
        };
        self.context_share_sequence = self.context_share_sequence.saturating_add(1);
        let now = Utc::now();
        let share_id = format!(
            "context:jenkins:{}:{}",
            now.timestamp_millis(),
            self.context_share_sequence
        );
        let mut share = match AgentContextShare::new_context_share(
            share_id,
            format!("Jenkins {} current-view context", self.active.label()),
            snapshot.binding.clone(),
            ActorRef {
                id: "jenkins-tui".to_string(),
                actor_type: ActorType::Human,
            },
            None,
            policy,
            now,
            now + chrono::Duration::seconds(DEFAULT_ASSIST_REQUEST_TTL_SECONDS),
        ) {
            Ok(share) => share,
            Err(error) => {
                self.status = format!("context share failed: {error}");
                return;
            }
        };
        share.preview = Some(snapshot.preview());
        if let Err(error) = share.transition_to(AgentContextShareStatus::Pending) {
            self.status = format!("context share failed: {error}");
            return;
        }
        let plugin_state = AssistPluginState {
            mode: mode_label(self.mode).to_string(),
            health: PluginSessionHealth::Ready,
            status: safe_error_summary(&self.status),
            updated_at: now,
            metadata: self.context_share_metadata(),
            redaction: RedactionStatus::Applied,
        };
        match self.context_share_store.share_with_owner_lease(
            share.clone(),
            snapshot,
            Some(plugin_state),
        ) {
            Ok((record, owner_lease)) => {
                if let Some(previous_id) = self
                    .context_share
                    .as_ref()
                    .map(|previous| previous.id.clone())
                {
                    let _ = self.context_share_store.cancel(&previous_id);
                }
                self.context_share_owner_lease = Some(owner_lease);
                if self.operation_confirmation.take().is_some() {
                    self.operation_plan = None;
                }
                self.context_share = Some(record.request);
                self.operation_request = None;
                self.operation_request_seen_id = None;
                self.last_operation_sync = Instant::now() - OPERATION_SYNC_INTERVAL;
                self.status = format!("Jenkins context shared: {}", share.id);
            }
            Err(error) => {
                self.status = format!("context share write failed: {error}");
            }
        }
    }

    fn sync_agent_operation(&mut self) -> bool {
        if self.last_operation_sync.elapsed() < OPERATION_SYNC_INTERVAL {
            return false;
        }
        self.last_operation_sync = Instant::now();
        let Some(share_id) = self.context_share.as_ref().map(|share| share.id.clone()) else {
            return false;
        };
        let detail = match self.context_share_store.detail(&share_id) {
            Ok(detail) => detail,
            Err(_) => {
                if self.operation_request.is_some() {
                    self.operation_request = None;
                    self.status = "agent operation synchronization unavailable".to_string();
                    return true;
                }
                return false;
            }
        };
        self.context_share = Some(detail.record.request.clone());
        if detail.record.request.status.is_terminal() {
            let changed = self.operation_request.take().is_some()
                || self.operation_confirmation.take().is_some();
            if changed {
                self.operation_plan = None;
                self.status = "context share ended; pending agent operation cleared".to_string();
            }
            return changed;
        }
        let Some(operation) = detail.record.latest_pending_operation_request().cloned() else {
            if self.operation_request.take().is_some() {
                self.status = "agent operation decision synchronized".to_string();
                return true;
            }
            return false;
        };
        if operation.actions.len() != 1 {
            if self.operation_request_seen_id.as_deref() == Some(operation.id.as_str()) {
                return false;
            }
            self.operation_request_seen_id = Some(operation.id);
            self.operation_request = None;
            self.status = "agent operation rejected: non-PTY review requires exactly one operation"
                .to_string();
            return true;
        }
        if self.operation_request_seen_id.as_deref() == Some(operation.id.as_str()) {
            return false;
        }
        self.operation_request_seen_id = Some(operation.id.clone());
        self.status = format!(
            "agent operation ready for y/n review: {}",
            operation.summary
        );
        self.operation_request = Some(operation);
        true
    }

    fn stage_agent_operation(&mut self) {
        let Some(operation_request) = self.operation_request.clone() else {
            self.status = "no agent operation to stage".to_string();
            return;
        };
        let operation = match operation_request.actions.as_slice() {
            [operation] => operation,
            _ => {
                self.status =
                    "agent operation rejected: expected exactly one operation".to_string();
                return;
            }
        };
        if let Err(error) = self.stage_agent_operation_action(operation) {
            self.status = format!("agent operation rejected: {error}");
            return;
        }
        let confirmation = match self.operation_confirmation(
            "staged_for_plan_review",
            "staged existing Jenkins operation plan; service command still requires plan confirmation",
        ) {
            Ok(confirmation) => confirmation,
            Err(error) => {
                self.operation_plan = None;
                self.status = format!("operation review failed: {error}");
                return;
            }
        };
        let share_id = confirmation.request_id.clone();
        match self
            .context_share_store
            .confirm_operation(&share_id, confirmation.clone())
        {
            Ok(_) => {
                self.operation_confirmation = Some(confirmation);
                self.operation_request = None;
                self.status = "agent operation staged; review the Jenkins plan".to_string();
            }
            Err(error) => {
                self.operation_plan = None;
                self.status = format!("operation decision write failed: {error}");
            }
        }
    }

    fn deny_agent_operation(&mut self) {
        let confirmation = match self.operation_confirmation(
            "denied_by_user",
            "operator denied the requested operation before plan staging",
        ) {
            Ok(confirmation) => confirmation,
            Err(error) => {
                self.status = format!("operation denial failed: {error}");
                return;
            }
        };
        let share_id = confirmation.request_id.clone();
        match self
            .context_share_store
            .confirm_operation(&share_id, confirmation.clone())
        {
            Ok(_) => {
                self.operation_confirmation = Some(confirmation);
                self.operation_request = None;
                self.status = "agent operation denied; no Jenkins plan was staged".to_string();
            }
            Err(error) => {
                self.status = format!("operation denial write failed: {error}");
            }
        }
    }

    fn operation_confirmation(
        &self,
        status: &str,
        note: &str,
    ) -> Result<AgentOperationConfirmation> {
        let share = self
            .context_share
            .as_ref()
            .context("no active context share")?;
        let operation_request = self
            .operation_request
            .as_ref()
            .context("no agent operation request")?;
        let operation = match operation_request.actions.as_slice() {
            [operation] => operation,
            _ => {
                return Err(anyhow!(
                    "agent operation request must contain exactly one operation"
                ));
            }
        };
        Ok(AgentOperationConfirmation {
            request_id: share.id.clone(),
            response_id: operation_request.id.clone(),
            action_index: 0,
            target: operation_target_label(operation),
            uses_current_pty: false,
            generation: share.binding.generation,
            confirmed_at: Utc::now(),
            expires_at: Some(share.expires_at),
            command_summary: operation_summary(operation),
            capability_id: operation_capability_id(operation),
            status: status.to_string(),
            note: note.to_string(),
            redaction: RedactionStatus::Applied,
        })
    }

    fn stage_agent_operation_action(&mut self, action: &AgentOperation) -> Result<()> {
        let AgentOperation::CapabilityCall {
            capability_id,
            input_summary,
            ..
        } = action
        else {
            return Err(anyhow!("agent operation is guidance only"));
        };
        let action_name = input_summary
            .get("action")
            .and_then(Value::as_str)
            .context("Jenkins operation requires action")?;
        let kind = match (capability_id.as_str(), action_name) {
            ("jenkins.trigger_build", "trigger") => OperationKind::TriggerBuild,
            ("jenkins.trigger_build", "retry") => OperationKind::RetryBuild,
            ("jenkins.abort_build", "abort") => OperationKind::AbortBuild,
            ("jenkins.cancel_queue_item", "cancel" | "cancel_queue") => OperationKind::CancelQueue,
            _ => {
                return Err(anyhow!(
                    "unsupported Jenkins operation {capability_id}/{action_name}"
                ));
            }
        };
        let selected = self.selected_item();
        let selected_job = selected.as_ref().map(|item| match item {
            BrowserItem::Job(job) => job.full_name.clone(),
            BrowserItem::Build(build) => build.job_full_name.clone(),
            BrowserItem::Running(build) => build.job_full_name.clone(),
            BrowserItem::Queue(item) => item.job_full_name.clone(),
        });
        let selected_build = selected.as_ref().and_then(|item| match item {
            BrowserItem::Build(build) => Some(build.number),
            BrowserItem::Running(build) => Some(build.build_number),
            _ => None,
        });
        let selected_queue = selected.as_ref().and_then(|item| match item {
            BrowserItem::Queue(item) => Some(item.id),
            _ => None,
        });

        let job_full_name = input_summary
            .get("job_full_name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(selected_job)
            .context("Jenkins operation requires job_full_name")?;
        let build_number = input_summary
            .get("build_number")
            .and_then(Value::as_u64)
            .or(selected_build);
        let queue_id = input_summary
            .get("queue_id")
            .and_then(Value::as_i64)
            .or(selected_queue);

        let target_label = input_summary
            .get("target_label")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| match kind {
                OperationKind::TriggerBuild => job_full_name.clone(),
                OperationKind::RetryBuild | OperationKind::AbortBuild => {
                    format!("{} #{}", job_full_name, build_number.unwrap_or_default())
                }
                OperationKind::CancelQueue => {
                    format!("queue item {}", queue_id.unwrap_or_default())
                }
            });

        let (build_number, queue_id, confirmations_required, risk) = match kind {
            OperationKind::TriggerBuild => (None, None, 1, "side_effecting"),
            OperationKind::RetryBuild => (build_number, None, 1, "side_effecting"),
            OperationKind::AbortBuild => (
                Some(build_number.context("Jenkins abort operation requires build_number")?),
                None,
                2,
                "destructive",
            ),
            OperationKind::CancelQueue => (
                None,
                Some(queue_id.context("Jenkins cancel operation requires queue_id")?),
                2,
                "destructive",
            ),
        };

        self.operation_plan = Some(OperationPlanView {
            kind,
            job_full_name,
            build_number,
            queue_id,
            target_label,
            risk,
            confirmations_required,
            confirmations: 0,
        });
        Ok(())
    }

    fn plan_trigger(&mut self) {
        let Some(BrowserItem::Job(job)) = self.selected_item() else {
            self.status = "select a job to trigger".to_string();
            return;
        };
        if !job.buildable {
            self.status = "selected job is not buildable".to_string();
            return;
        }
        self.operation_plan = Some(OperationPlanView {
            kind: OperationKind::TriggerBuild,
            job_full_name: job.full_name.clone(),
            build_number: None,
            queue_id: None,
            target_label: job.full_name,
            risk: "side_effecting",
            confirmations_required: 1,
            confirmations: 0,
        });
        self.status =
            "trigger plan staged; parameters preview is empty for this fixture".to_string();
    }

    fn plan_retry(&mut self) {
        let Some(BrowserItem::Build(build)) = self.selected_item() else {
            self.status = "select a completed build to retry".to_string();
            return;
        };
        self.operation_plan = Some(OperationPlanView {
            kind: OperationKind::RetryBuild,
            job_full_name: build.job_full_name.clone(),
            build_number: Some(build.number),
            queue_id: None,
            target_label: format!("{} #{}", build.job_full_name, build.number),
            risk: "side_effecting",
            confirmations_required: 1,
            confirmations: 0,
        });
        self.status = "retry plan staged; press y".to_string();
    }

    fn plan_stop_or_cancel(&mut self) {
        match self.selected_item() {
            Some(BrowserItem::Build(build)) if build.building => {
                self.stage_abort(build.job_full_name, build.number);
            }
            Some(BrowserItem::Running(build)) => {
                self.stage_abort(build.job_full_name, build.build_number);
            }
            Some(BrowserItem::Queue(item)) => {
                self.operation_plan = Some(OperationPlanView {
                    kind: OperationKind::CancelQueue,
                    job_full_name: item.job_full_name,
                    build_number: None,
                    queue_id: Some(item.id),
                    target_label: format!("queue item {}", item.id),
                    risk: "destructive",
                    confirmations_required: 2,
                    confirmations: 0,
                });
                self.status = "cancel queue plan staged; press y twice".to_string();
            }
            _ => self.status = "select a running build or queue item to stop".to_string(),
        }
    }

    fn stage_abort(&mut self, job_full_name: String, build_number: u64) {
        self.operation_plan = Some(OperationPlanView {
            kind: OperationKind::AbortBuild,
            job_full_name: job_full_name.clone(),
            build_number: Some(build_number),
            queue_id: None,
            target_label: format!("{job_full_name} #{build_number}"),
            risk: "destructive",
            confirmations_required: 2,
            confirmations: 0,
        });
        self.status = "abort plan staged; press y twice".to_string();
    }

    fn confirm_plan(&mut self) {
        if !self.agent_operation_plan_is_current() {
            return;
        }
        let Some(mut plan) = self.operation_plan.take() else {
            self.status = "no operation plan to confirm".to_string();
            return;
        };
        plan.confirmations = plan.confirmations.saturating_add(1);
        if plan.confirmations < plan.confirmations_required {
            self.status = format!(
                "{} on {} is {}; press y again to execute",
                plan.kind.label(),
                plan.target_label,
                plan.risk
            );
            self.operation_plan = Some(plan);
            return;
        }
        if self.readonly {
            self.status = format!(
                "readonly launch blocked {} on {}",
                plan.kind.label(),
                plan.target_label
            );
            self.operation_plan = Some(plan);
            return;
        }
        if let Some(service) = &self.service {
            match plan.kind {
                OperationKind::TriggerBuild | OperationKind::RetryBuild => {
                    service.send(JenkinsCommand::TriggerBuild {
                        job_full_name: plan.job_full_name,
                    });
                }
                OperationKind::AbortBuild => {
                    service.send(JenkinsCommand::AbortBuild {
                        job_full_name: plan.job_full_name,
                        build_number: plan.build_number.unwrap_or_default(),
                    });
                }
                OperationKind::CancelQueue => {
                    service.send(JenkinsCommand::CancelQueueItem {
                        queue_id: plan.queue_id.unwrap_or_default(),
                    });
                }
            }
            self.status = "operation sent to JenkinsService".to_string();
        } else {
            self.status = format!(
                "fixture executed {} on {}",
                plan.kind.label(),
                plan.target_label
            );
        }
        self.operation_confirmation = None;
    }

    fn cancel_plan(&mut self) {
        if self.operation_plan.take().is_some() {
            self.operation_confirmation = None;
            self.status = "operation plan cancelled".to_string();
        } else {
            self.mode = Mode::Browser;
            self.status = "browser mode".to_string();
        }
    }

    fn agent_operation_plan_is_current(&mut self) -> bool {
        let Some(confirmation) = self.operation_confirmation.as_ref() else {
            return true;
        };
        let now = Utc::now();
        let current = self.context_share.as_ref().is_some_and(|share| {
            !share.status.is_terminal()
                && share.binding.generation == confirmation.generation
                && share.expires_at > now
                && confirmation
                    .expires_at
                    .is_none_or(|expires_at| expires_at > now)
        });
        if current {
            return true;
        }
        self.operation_plan = None;
        self.operation_confirmation = None;
        self.status = "agent operation plan expired or became stale; plan cleared".to_string();
        false
    }

    fn show_help(&mut self) {
        self.return_mode = self.mode;
        self.mode = Mode::Help;
        self.status = "help open".to_string();
    }

    fn trim_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.filtered_items().len().saturating_sub(1));
    }

    fn filtered_items(&self) -> Vec<BrowserItem> {
        let filter = self.filter.to_lowercase();
        self.all_items()
            .into_iter()
            .filter(|item| filter.is_empty() || item.search_text().contains(&filter))
            .collect()
    }

    fn all_items(&self) -> Vec<BrowserItem> {
        match self.active {
            ViewKind::Jobs => self.jobs.iter().cloned().map(BrowserItem::Job).collect(),
            ViewKind::Builds => self
                .builds
                .iter()
                .cloned()
                .map(BrowserItem::Build)
                .collect(),
            ViewKind::Activity => self
                .running
                .iter()
                .cloned()
                .map(BrowserItem::Running)
                .chain(self.queue.iter().cloned().map(BrowserItem::Queue))
                .collect(),
        }
    }

    fn selected_item(&self) -> Option<BrowserItem> {
        self.filtered_items().get(self.selected).cloned()
    }

    fn filter_label(&self) -> String {
        if self.filter.is_empty() {
            "none".to_string()
        } else {
            self.filter.clone()
        }
    }

    fn build_context_share_snapshot(
        &self,
        policy: &AssistContextPolicy,
    ) -> std::result::Result<AssistContextSnapshot, voidb_core::AssistContractError> {
        policy.validate()?;
        let descriptor = PluginSessionRegistration::new(
            "jenkins",
            format!("jenkins-tui:{}", self.profile_label),
            PluginSessionPurpose::InfrastructureClient,
            PluginSessionScope::LocalProcess,
        )
        .with_health(PluginSessionHealth::Ready)
        .with_authenticated(matches!(
            self.source,
            JenkinsTuiSource::Profile | JenkinsTuiSource::Connection
        ))
        .with_destructive_capable(!self.readonly)
        .with_stream_capable(true)
        .with_metadata(self.context_share_metadata(), RedactionStatus::Applied)
        .descriptor;
        let status_line = AssistBoundedText::capture(
            &safe_error_summary(&self.status),
            512,
            RedactionStatus::Applied,
        )?;
        let metadata_text =
            serde_json::to_string(&self.context_share_metadata()).map_err(|_| {
                voidb_core::AssistContractError::InvalidRequest(
                    "failed to encode Jenkins context-share metadata".to_string(),
                )
            })?;
        let transcript_tail = AssistBoundedText::capture(
            &metadata_text,
            policy.metadata_bytes,
            RedactionStatus::Applied,
        )?;
        Ok(AssistContextSnapshot {
            binding: AssistSessionBinding::from_descriptor(&descriptor),
            captured_at: Utc::now(),
            mode: format!("jenkins:{}", self.active.label()),
            health: PluginSessionHealth::Ready,
            terminal: None,
            visible_screen: None,
            transcript_tail: Some(transcript_tail),
            status_line: Some(status_line),
            withheld_fields: vec![
                AssistWithheldField {
                    field: "jenkins.client_handle".to_string(),
                    reason: AssistWithholdingReason::Policy,
                },
                AssistWithheldField {
                    field: "jenkins.auth".to_string(),
                    reason: AssistWithholdingReason::SecretMaterial,
                },
                AssistWithheldField {
                    field: "jenkins.console.secret_lines".to_string(),
                    reason: AssistWithholdingReason::SensitiveMetadata,
                },
            ],
            metadata: self.context_share_metadata(),
            redaction: RedactionStatus::Applied,
        })
    }

    fn context_share_metadata(&self) -> Value {
        json!({
            "profile_label": self.profile_label,
            "server_label": self.server_label,
            "auth_method": self.auth_method,
            "source": self.source,
            "readonly": self.readonly,
            "active_view": self.active.label(),
            "selected": self.selected_item().map(|item| item.safe_label()),
            "counts": {
                "jobs": self.jobs.len(),
                "builds": self.builds.len(),
                "running": self.running.len(),
                "queue": self.queue.len()
            },
            "console": {
                "following": self.console_follow.as_ref().map(|(job, build)| {
                    format!("{job} #{build}")
                }),
                "offset": self.console_offset,
                "has_more": self.console_has_more,
                "lines": self.console.len(),
                "dropped_lines": self.console.dropped_lines,
                "bytes": self.console.bytes
            },
            "withheld": [
                "jenkins.client_handle",
                "jenkins.auth",
                "jenkins.console.secret_lines",
                "jenkins.profile_config"
            ]
        })
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(12),
                Constraint::Length(8),
                Constraint::Length(3),
            ])
            .split(area);

        frame.render_widget(self.header(), vertical[0]);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(vertical[1]);
        frame.render_widget(self.browser_list(body[0]), body[0]);
        frame.render_widget(self.detail_panel(), body[1]);
        frame.render_widget(self.console_panel(), vertical[2]);
        frame.render_widget(self.status_panel(), vertical[3]);

        match self.mode {
            Mode::Help => self.draw_help(frame, area),
            Mode::Error => self.draw_error(frame, area),
            Mode::Browser | Mode::Filter => {}
        }
    }

    fn header(&self) -> Paragraph<'_> {
        let source = format!("{:?}", self.source).to_lowercase();
        let tabs = ViewKind::ALL
            .iter()
            .map(|kind| {
                if *kind == self.active {
                    format!("[{}]", kind.label())
                } else {
                    kind.label().to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Jenkins Builds",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  profile={} server={} auth={}",
                    self.profile_label, self.server_label, self.auth_method
                )),
            ]),
            Line::from(format!(
                "{}  source={} purpose={} readonly={} restore={} search={} console_follow={}",
                tabs,
                source,
                self.purpose,
                self.readonly,
                self.restore,
                self.filter_label(),
                self.console_follow.is_some()
            )),
        ])
        .block(Block::default().borders(Borders::ALL))
    }

    fn browser_list(&self, area: Rect) -> Paragraph<'_> {
        let items = self.filtered_items();
        let visible = area.height.saturating_sub(2).max(1) as usize;
        let start = window_start(self.selected, visible, items.len());
        let lines = items
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(idx, item)| {
                let marker = if idx == self.selected { ">" } else { " " };
                let style = if idx == self.selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(
                    format!("{marker} {}", item.list_label()),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        Paragraph::new(if lines.is_empty() {
            vec![Line::from("No items loaded. Press r to refresh.")]
        } else {
            lines
        })
        .block(
            Block::default()
                .title(format!(
                    " {} ({}/{}) ",
                    self.active.label(),
                    self.selected.saturating_add(1).min(items.len()),
                    items.len()
                ))
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false })
    }

    fn detail_panel(&self) -> Paragraph<'_> {
        let mut lines = match self.selected_item() {
            Some(BrowserItem::Job(job)) => vec![
                Line::from(format!("job: {}", job.full_name)),
                Line::from(format!("status: {}", job.status)),
                Line::from(format!(
                    "buildable: {}  in_queue: {}",
                    job.buildable, job.in_queue
                )),
                Line::from(format!("folder: {}", job.folder)),
                Line::from(format!("description: {}", job.description)),
            ],
            Some(BrowserItem::Build(build)) => vec![
                Line::from(format!("build: {} #{}", build.job_full_name, build.number)),
                Line::from(format!(
                    "result: {}  building: {}",
                    build.result, build.building
                )),
                Line::from(format!("duration_ms: {}", build.duration)),
            ],
            Some(BrowserItem::Running(build)) => vec![
                Line::from(format!(
                    "running: {} #{}",
                    build.job_full_name, build.build_number
                )),
                Line::from(format!("node: {} executor: {}", build.node, build.executor)),
                Line::from(format!(
                    "progress: {}",
                    build
                        .progress
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string())
                )),
            ],
            Some(BrowserItem::Queue(item)) => vec![
                Line::from(format!("queue item: {}", item.id)),
                Line::from(format!("job: {}", item.job_full_name)),
                Line::from(format!("blocked: {} stuck: {}", item.blocked, item.stuck)),
                Line::from(format!("why: {}", item.why)),
            ],
            None => vec![Line::from(
                "Select a job, build, running build, or queue item.",
            )],
        };

        if let Some(plan) = &self.operation_plan {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "operation plan",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(format!("action: {}", plan.kind.label())));
            lines.push(Line::from(format!("target: {}", plan.target_label)));
            lines.push(Line::from(format!("risk: {}", plan.risk)));
            lines.push(Line::from(format!(
                "confirmations: {}/{}",
                plan.confirmations, plan.confirmations_required
            )));
        }

        Paragraph::new(lines)
            .block(Block::default().title(" Details ").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    fn console_panel(&self) -> Paragraph<'_> {
        let mut lines = self
            .console
            .tail(6)
            .into_iter()
            .map(Line::from)
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push(Line::from("No console data. Select a build and press f."));
        }
        if self.console.dropped_lines > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "dropped {} console lines due to bounds",
                    self.console.dropped_lines
                ),
                Style::default().fg(Color::Yellow),
            )));
        }
        Paragraph::new(lines)
            .block(Block::default().title(" Console ").borders(Borders::ALL))
            .wrap(Wrap { trim: false })
    }

    fn status_panel(&self) -> Paragraph<'_> {
        Paragraph::new(vec![Line::from(format!(
            "{} | mode={} | jobs={} builds={} running={} queue={} console {} lines/{} dropped",
            self.primary_status(),
            mode_label(self.mode),
            self.jobs.len(),
            self.builds.len(),
            self.running.len(),
            self.queue.len(),
            self.console.len(),
            self.console.dropped_lines
        ))])
        .block(Block::default().title(" Status ").borders(Borders::ALL))
    }

    fn primary_status(&self) -> String {
        self.operation_request
            .as_ref()
            .map(|request| {
                format!(
                    "agent operation: {} [y stage, n deny]",
                    safe_error_summary(&request.summary)
                )
            })
            .unwrap_or_else(|| self.status.clone())
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(72, 56, area);
        frame.render_widget(Clear, popup);
        let help = vec![
            Line::from("Jenkins build and console TUI"),
            Line::from("Tab / Shift+Tab: switch jobs, builds, activity"),
            Line::from("j/k: move  /: search  r: refresh  Enter: detail/builds"),
            Line::from("f: follow console  c: cancel console follow"),
            Line::from("a: share bounded current-view context"),
            Line::from("pending agent operation: y stage plan  n deny"),
            Line::from("t: trigger job  R: retry build  x: abort build or cancel queue"),
            Line::from("y: confirm plan  Esc: cancel plan  q: quit"),
        ];
        frame.render_widget(
            Paragraph::new(help)
                .block(Block::default().title(" Help ").borders(Borders::ALL))
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false }),
            popup,
        );
    }

    fn draw_error(&self, frame: &mut Frame, area: Rect) {
        let popup = centered_rect(72, 40, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Jenkins server error",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(self.status.clone()),
                Line::from("Press any key to return to the browser."),
            ])
            .block(Block::default().title(" Error ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

impl BoundedLines {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            dropped_lines: 0,
            max_lines,
            max_bytes,
        }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
        self.dropped_lines = 0;
    }

    fn push_text(&mut self, text: &str) {
        for line in text.lines() {
            self.push_line(line.to_string());
        }
    }

    fn push_line(&mut self, mut line: String) {
        if line.len() > self.max_bytes {
            line.truncate(self.max_bytes);
        }
        self.bytes = self.bytes.saturating_add(line.len());
        self.lines.push_back(line);
        while self.lines.len() > self.max_lines || self.bytes > self.max_bytes {
            if let Some(removed) = self.lines.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.len());
                self.dropped_lines = self.dropped_lines.saturating_add(1);
            } else {
                break;
            }
        }
    }

    fn tail(&self, count: usize) -> Vec<String> {
        self.lines
            .iter()
            .rev()
            .take(count)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

impl ViewKind {
    const ALL: [ViewKind; 3] = [ViewKind::Jobs, ViewKind::Builds, ViewKind::Activity];

    fn label(self) -> &'static str {
        match self {
            ViewKind::Jobs => "jobs",
            ViewKind::Builds => "builds",
            ViewKind::Activity => "activity",
        }
    }
}

impl BrowserItem {
    fn list_label(&self) -> String {
        match self {
            BrowserItem::Job(job) => format!(
                "{:<10} {:<36} {}",
                job.status, job.full_name, job.description
            ),
            BrowserItem::Build(build) => format!(
                "#{:<8} {:<10} building={} duration={}ms",
                build.number, build.result, build.building, build.duration
            ),
            BrowserItem::Running(build) => format!(
                "RUNNING {:<34} #{} node={} executor={}",
                build.job_full_name, build.build_number, build.node, build.executor
            ),
            BrowserItem::Queue(item) => format!(
                "QUEUE {:<36} id={} {}",
                item.job_full_name, item.id, item.why
            ),
        }
    }

    fn safe_label(&self) -> String {
        match self {
            BrowserItem::Job(job) => job.full_name.clone(),
            BrowserItem::Build(build) => format!("{} #{}", build.job_full_name, build.number),
            BrowserItem::Running(build) => {
                format!("{} #{}", build.job_full_name, build.build_number)
            }
            BrowserItem::Queue(item) => format!("queue item {}", item.id),
        }
    }

    fn search_text(&self) -> String {
        match self {
            BrowserItem::Job(job) => format!(
                "{} {} {} {}",
                job.name, job.full_name, job.status, job.description
            ),
            BrowserItem::Build(build) => {
                format!("{} {} {}", build.job_full_name, build.number, build.result)
            }
            BrowserItem::Running(build) => {
                format!("{} {} {}", build.job_name, build.job_full_name, build.node)
            }
            BrowserItem::Queue(item) => {
                format!("{} {} {}", item.job_name, item.job_full_name, item.why)
            }
        }
        .to_lowercase()
    }
}

impl OperationKind {
    fn label(&self) -> &'static str {
        match self {
            OperationKind::TriggerBuild => "trigger build",
            OperationKind::RetryBuild => "retry build",
            OperationKind::AbortBuild => "abort build",
            OperationKind::CancelQueue => "cancel queue item",
        }
    }
}

fn server_value(config: &JenkinsConfig) -> Value {
    json!({
        "server_label": redacted_url(config.base_url()),
        "auth_method": auth_label(&config.auth),
        "timeout_seconds": config.timeout,
        "verify_ssl": config.verify_ssl,
        "secret_material": "redacted"
    })
}

fn auth_label(auth: &JenkinsAuth) -> &'static str {
    match auth {
        JenkinsAuth::None => "none",
        JenkinsAuth::Basic { .. } => "basic",
    }
}

fn redacted_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    if let Some((scheme, rest)) = without_query.split_once("://")
        && let Some((_, host_path)) = rest.rsplit_once('@')
    {
        return format!("{scheme}://redacted@{host_path}");
    }
    without_query.to_string()
}

fn redact_console(text: &str) -> String {
    text.lines()
        .map(|line| {
            let lower = line.to_lowercase();
            if lower.contains("token=")
                || lower.contains("password=")
                || lower.contains("secret=")
                || lower.contains("cookie:")
            {
                "[redacted console line]".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn job_value(job: &JobView) -> Value {
    json!({
        "name": job.name,
        "full_name": job.full_name,
        "status": job.status,
        "folder": job.folder,
        "buildable": job.buildable,
        "in_queue": job.in_queue
    })
}

fn build_value(build: &BuildView) -> Value {
    json!({
        "job_full_name": build.job_full_name,
        "number": build.number,
        "result": build.result,
        "building": build.building,
        "duration": build.duration
    })
}

fn running_value(build: &RunningView) -> Value {
    json!({
        "job_name": build.job_name,
        "job_full_name": build.job_full_name,
        "build_number": build.build_number,
        "node": build.node,
        "executor": build.executor,
        "progress": build.progress
    })
}

fn queue_value(item: &QueueView) -> Value {
    json!({
        "id": item.id,
        "job_name": item.job_name,
        "job_full_name": item.job_full_name,
        "why": item.why,
        "blocked": item.blocked,
        "stuck": item.stuck
    })
}

fn safe_error_summary(message: &str) -> String {
    let mut summary = message.replace('\n', " ");
    if summary.len() > 180 {
        summary.truncate(177);
        summary.push_str("...");
    }
    summary
}

fn secret_leak_markers(text: &str) -> Vec<String> {
    [
        "jenkins_api_token_value",
        "crumb_secret_value",
        "jenkins_cookie_value",
        "secret_parameter_value",
        "raw_jenkins_config",
    ]
    .into_iter()
    .filter(|marker| text.contains(marker))
    .map(str::to_string)
    .collect()
}

fn jenkins_context_share_store() -> Result<AgentContextShareStore> {
    AgentContextShareStore::new(
        jenkins_agent_context_store_root()?,
        AgentContextSharePolicy::non_pty(),
    )
}

pub fn jenkins_agent_context_store_root() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os(JENKINS_AGENT_CONTEXT_STORE_DIR_ENV)
        .or_else(|| std::env::var_os(LEGACY_JENKINS_ASSIST_STORE_DIR_ENV))
    {
        Ok(std::path::PathBuf::from(path))
    } else {
        Ok(AppConfig::config_dir()
            .map_err(|error| anyhow!(error.to_string()))?
            .join("jenkins-assist"))
    }
}

fn operation_target_label(action: &AgentOperation) -> String {
    match action {
        AgentOperation::CapabilityCall {
            capability_id,
            input_summary,
            ..
        } => {
            let target = input_summary
                .get("target_label")
                .and_then(Value::as_str)
                .unwrap_or("jenkins-target");
            format!("capability:{capability_id}:{target}")
        }
        AgentOperation::Guidance { title, .. } => format!("guidance:{title}"),
        AgentOperation::RequestPermission { permission, .. } => {
            format!("permission:{permission:?}")
        }
        AgentOperation::ProposedCommand { target, .. } => format!("command:{target:?}"),
    }
}

fn operation_summary(action: &AgentOperation) -> Option<String> {
    match action {
        AgentOperation::CapabilityCall {
            capability_id,
            input_summary,
            ..
        } => Some(format!(
            "{capability_id} {}",
            safe_error_summary(&input_summary.to_string())
        )),
        AgentOperation::ProposedCommand { command, .. } => Some(safe_error_summary(command)),
        _ => None,
    }
}

fn operation_capability_id(action: &AgentOperation) -> Option<String> {
    match action {
        AgentOperation::CapabilityCall { capability_id, .. } => Some(capability_id.clone()),
        AgentOperation::ProposedCommand {
            target: AgentOperationTarget::Capability { capability_id },
            ..
        } => Some(capability_id.clone()),
        _ => None,
    }
}

fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Browser => "browser",
        Mode::Filter => "filter",
        Mode::Help => "help",
        Mode::Error => "error",
    }
}

fn window_start(selected: usize, visible: usize, len: usize) -> usize {
    if visible == 0 || len <= visible {
        0
    } else if selected >= visible {
        (selected + 1).saturating_sub(visible)
    } else {
        0
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

struct StandaloneJenkinsTabManager {
    render_tx: mpsc::UnboundedSender<()>,
    should_quit: Arc<AtomicBool>,
}

impl StandaloneJenkinsTabManager {
    fn new(should_quit: Arc<AtomicBool>) -> Self {
        let (render_tx, _render_rx) = mpsc::unbounded_channel();
        Self {
            render_tx,
            should_quit,
        }
    }

    fn unsupported_tabs_error() -> anyhow::Error {
        anyhow!("Jenkins TUI does not host plugin tabs; use plugin-owned CLI commands instead")
    }
}

impl TabManager for StandaloneJenkinsTabManager {
    fn open(&self, _title: String, _plugin_id: String, _context: Value) -> Result<()> {
        Err(Self::unsupported_tabs_error())
    }

    fn close_current(&self) -> Result<()> {
        self.quit()
    }

    fn set_title(&self, _title: String) -> Result<()> {
        Ok(())
    }

    fn request_render(&self) -> Result<()> {
        let _ = self.render_tx.send(());
        Ok(())
    }

    fn list_tabs(&self) -> Result<Vec<TabInfo>> {
        Ok(vec![TabInfo {
            index: 0,
            title: "Jenkins".to_string(),
            plugin_id: "jenkins".to_string(),
            context: json!({}),
            is_active: true,
        }])
    }

    fn close_tab(&self, index: usize) -> Result<()> {
        if index == 0 {
            self.quit()
        } else {
            Err(anyhow!("Jenkins TUI has no tab {index}"))
        }
    }

    fn switch_to(&self, index: usize) -> Result<()> {
        if index == 0 {
            Ok(())
        } else {
            Err(anyhow!("Jenkins TUI has no tab {index}"))
        }
    }

    fn active_tab_index(&self) -> Result<usize> {
        Ok(0)
    }

    fn quit(&self) -> Result<()> {
        self.should_quit.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_launch(readonly: bool) -> JenkinsTuiLaunch {
        JenkinsTuiLaunch {
            profile_label: "fixture".to_string(),
            config: None,
            source: JenkinsTuiSource::Fixture,
            fixture_path: Some(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("fixtures/jenkins_tui_operations.json")
                    .display()
                    .to_string(),
            ),
            purpose: "build-triage".to_string(),
            readonly,
            restore: true,
            launch_plan: None,
        }
    }

    #[test]
    fn preflight_redacts_basic_auth_token() {
        let launch = JenkinsTuiLaunch {
            profile_label: "jenkins".to_string(),
            config: Some(JenkinsConfig {
                url: "https://user:jenkins_api_token_value@jenkins.example.test?crumb=crumb_secret_value".to_string(),
                auth: JenkinsAuth::Basic {
                    username: "builder".to_string(),
                    token: "jenkins_api_token_value".to_string(),
                },
                timeout: 20,
                verify_ssl: true,
            }),
            source: JenkinsTuiSource::Connection,
            fixture_path: None,
            purpose: "build-triage".to_string(),
            readonly: false,
            restore: true,
            launch_plan: None,
        };
        let rendered = serde_json::to_string(&preflight_value(&launch)).unwrap();
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains("jenkins_api_token_value"));
        assert!(!rendered.contains("crumb_secret_value"));
    }

    #[test]
    fn fixture_evidence_has_coverage_and_no_secret_markers() {
        let evidence = build_jenkins_tui_evidence(&fixture_launch(false)).unwrap();
        let rendered = serde_json::to_string(&evidence).unwrap();
        assert!(rendered.contains("jenkins_tui_fixture_evidence"));
        assert!(rendered.contains("stop_double_confirmation"));
        assert!(rendered.contains("current_pty_rejected"));
        assert!(!rendered.contains("assist_handoff"));
        assert!(!rendered.contains("response_review"));
        assert_eq!(
            evidence["external_agent_interaction"]["broker_policy"],
            json!("non_pty")
        );
        assert_eq!(evidence["secret_leak_scan"]["passed"], json!(true));
    }

    #[test]
    fn search_matches_failed_job() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.filter = "failed".to_string();
        let items = app.filtered_items();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn abort_plan_requires_second_confirmation() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.active = ViewKind::Activity;
        app.plan_stop_or_cancel();
        app.confirm_plan();
        assert!(app.operation_plan.is_some());
        assert!(app.status.contains("press y again"));
        app.confirm_plan();
        assert!(app.operation_plan.is_none());
        assert!(app.status.contains("fixture executed abort build"));
    }

    #[test]
    fn readonly_launch_blocks_confirmed_plan() {
        let mut app = JenkinsTuiApp::new(fixture_launch(true)).unwrap();
        app.active = ViewKind::Activity;
        app.plan_stop_or_cancel();
        app.confirm_plan();
        app.confirm_plan();
        assert!(app.operation_plan.is_some());
        assert!(app.status.contains("readonly launch blocked"));
    }

    #[test]
    fn external_agent_operation_stages_existing_jenkins_confirmation_plan() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.context_share_store = temp_context_share_store();
        app.share_agent_context();
        let request_id = app.context_share.as_ref().unwrap().id.clone();
        let target = match app.selected_item().unwrap() {
            BrowserItem::Job(job) => job,
            _ => panic!("fixture starts with a job selected"),
        };
        app.context_share_store
            .post_operation_request(&request_id, jenkins_operation_request(&request_id, &target))
            .unwrap();

        app.sync_agent_operation();
        assert!(app.status.contains("agent operation ready"));
        assert!(app.primary_status().contains("[y stage, n deny]"));
        app.stage_agent_operation();
        assert!(app.operation_plan.is_some());
        assert!(app.operation_confirmation.is_some());
        let detail = app.context_share_store.detail(&request_id).unwrap();
        assert_eq!(detail.record.action_confirmations.len(), 1);
        assert!(!detail.record.action_confirmations[0].uses_current_pty);

        app.confirm_plan();
        assert!(app.operation_plan.is_none());
        assert!(app.status.contains("fixture executed trigger build"));
    }

    #[test]
    fn external_agent_operation_can_be_denied_without_staging() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.context_share_store = temp_context_share_store();
        app.share_agent_context();
        let request_id = app.context_share.as_ref().unwrap().id.clone();
        let target = match app.selected_item().unwrap() {
            BrowserItem::Job(job) => job,
            _ => panic!("fixture starts with a job selected"),
        };
        app.context_share_store
            .post_operation_request(&request_id, jenkins_operation_request(&request_id, &target))
            .unwrap();

        app.sync_agent_operation();
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        assert!(app.operation_plan.is_none());
        assert!(app.status.contains("denied"));
        let detail = app.context_share_store.detail(&request_id).unwrap();
        assert_eq!(
            detail.record.action_confirmations[0].status,
            "denied_by_user"
        );
    }

    #[test]
    fn external_multi_action_request_is_never_partially_staged() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.context_share_store =
            temp_context_share_store_with_policy(AgentContextSharePolicy::current_pty_capable());
        app.share_agent_context();
        let request_id = app.context_share.as_ref().unwrap().id.clone();
        let target = match app.selected_item().unwrap() {
            BrowserItem::Job(job) => job,
            _ => panic!("fixture starts with a job selected"),
        };
        let mut request = jenkins_operation_request(&request_id, &target);
        request.actions.push(request.actions[0].clone());
        app.context_share_store
            .post_operation_request(&request_id, request)
            .unwrap();

        assert!(app.sync_agent_operation());
        assert!(app.operation_request.is_none());
        assert!(app.operation_plan.is_none());
        assert!(app.status.contains("exactly one operation"));
        assert!(
            app.context_share_store
                .detail(&request_id)
                .unwrap()
                .record
                .action_confirmations
                .is_empty()
        );
    }

    #[test]
    fn expired_agent_plan_is_cleared_before_service_dispatch() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.context_share_store = temp_context_share_store();
        app.share_agent_context();
        let request_id = app.context_share.as_ref().unwrap().id.clone();
        let target = match app.selected_item().unwrap() {
            BrowserItem::Job(job) => job,
            _ => panic!("fixture starts with a job selected"),
        };
        app.context_share_store
            .post_operation_request(&request_id, jenkins_operation_request(&request_id, &target))
            .unwrap();
        app.sync_agent_operation();
        app.stage_agent_operation();
        app.context_share.as_mut().unwrap().expires_at = Utc::now() - chrono::Duration::seconds(1);

        app.confirm_plan();
        assert!(app.operation_plan.is_none());
        assert!(app.operation_confirmation.is_none());
        assert!(app.status.contains("expired or became stale"));
    }

    #[test]
    fn uppercase_a_is_not_an_operation_shortcut() {
        let mut app = JenkinsTuiApp::new(fixture_launch(false)).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
        assert!(app.status.contains("a share context"));
        assert!(!app.status.contains("assist"));
        assert!(!app.status.contains("poll"));
    }

    fn temp_context_share_store() -> AgentContextShareStore {
        temp_context_share_store_with_policy(AgentContextSharePolicy::non_pty())
    }

    fn temp_context_share_store_with_policy(
        policy: AgentContextSharePolicy,
    ) -> AgentContextShareStore {
        AgentContextShareStore::new(
            std::env::temp_dir().join(format!(
                "voidb-jenkins-agent-context-test-{}",
                uuid::Uuid::new_v4()
            )),
            policy,
        )
        .unwrap()
    }

    fn jenkins_operation_request(request_id: &str, target: &JobView) -> AgentOperationRequest {
        AgentOperationRequest {
            id: format!("agent:operation:jenkins:{}", uuid::Uuid::new_v4()),
            request_id: request_id.to_string(),
            agent: AgentPrincipal {
                client_id: "agent".to_string(),
                task_id: "tw-90".to_string(),
                instance_id: Some("jenkins-agent-operation-test".to_string()),
            },
            created_at: Utc::now(),
            summary: "Trigger the selected Jenkins job after operator review.".to_string(),
            diagnosis: Some("Fixture job is buildable and selected for plan review.".to_string()),
            actions: vec![AgentOperation::CapabilityCall {
                capability_id: "jenkins.trigger_build".to_string(),
                input_summary: json!({
                    "action": "trigger",
                    "job_full_name": target.full_name.clone(),
                    "target_label": target.full_name.clone()
                }),
                rationale: "Use the existing Jenkins trigger plan and confirmation gate."
                    .to_string(),
                risk: AgentOperationRisk::Review,
                target: AgentOperationTarget::Capability {
                    capability_id: "jenkins.trigger_build".to_string(),
                },
            }],
            requested_permissions: vec![AssistPermission::ProposeCommands],
            redaction: RedactionStatus::Applied,
        }
    }
}
