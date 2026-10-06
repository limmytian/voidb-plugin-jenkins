//! Shared Jenkins domain types.
//!
//! Author: Limmy

use serde::{Deserialize, Serialize};

/// A Jenkins job summary (as returned by the root API listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    /// Job name (display name).
    pub name: String,
    /// Full URL to the job.
    pub url: String,
    /// Raw color field from Jenkins (`blue`, `red`, `yellow`, `disabled`, ...).
    /// `None` for folders / container jobs.
    #[serde(default)]
    pub color: Option<String>,
    /// The Jenkins `_class` field, used to detect folders.
    #[serde(default, rename = "_class")]
    pub class: Option<String>,
}

impl JobSummary {
    /// Whether this job node is a container (folder / multibranch / org).
    pub fn is_folder(&self) -> bool {
        if self.color.is_some() {
            return false;
        }
        match self.class.as_deref() {
            Some(c) => {
                c.contains("Folder")
                    || c.contains("MultiBranchProject")
                    || c.contains("OrganizationFolder")
                    || c.contains("WorkflowMultiBranchProject")
            }
            None => false,
        }
    }

    /// Get a short, human-readable status label derived from `color`.
    pub fn status_label(&self) -> &'static str {
        match self.color.as_deref().unwrap_or("") {
            "blue" | "blue_anime" => "SUCCESS",
            "red" | "red_anime" => "FAILED",
            "yellow" | "yellow_anime" => "UNSTABLE",
            "aborted" | "aborted_anime" => "ABORTED",
            "notbuilt" | "notbuilt_anime" => "NOT BUILT",
            "disabled" | "disabled_anime" => "DISABLED",
            c if c.ends_with("_anime") => "RUNNING",
            "" if self.is_folder() => "FOLDER",
            _ => "UNKNOWN",
        }
    }

    /// Whether the job is currently running (color ends with `_anime`).
    pub fn is_running(&self) -> bool {
        self.color
            .as_deref()
            .map(|c| c.ends_with("_anime"))
            .unwrap_or(false)
    }
}

/// A single build entry on a job.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildSummary {
    pub number: u64,
    #[serde(default)]
    pub url: String,
    /// One of `SUCCESS`, `FAILURE`, `UNSTABLE`, `ABORTED`, `NOT_BUILT`, or `None` while running.
    #[serde(default)]
    pub result: Option<String>,
    /// Build start time (milliseconds since epoch).
    #[serde(default)]
    pub timestamp: u64,
    /// Build duration in milliseconds (0 while running).
    #[serde(default)]
    pub duration: u64,
    /// Whether the build is still running.
    #[serde(default)]
    pub building: bool,
}

impl BuildSummary {
    /// Returns a display label for the build result.
    pub fn result_label(&self) -> &str {
        if self.building {
            return "RUNNING";
        }
        self.result.as_deref().unwrap_or("UNKNOWN")
    }
}

/// Detailed info about a job: metadata + recent builds.
#[derive(Debug, Clone, Default)]
pub struct JobDetail {
    pub name: String,
    pub full_name: String,
    pub description: String,
    pub url: String,
    pub buildable: bool,
    pub in_queue: bool,
    pub builds: Vec<BuildSummary>,
}

/// A chunk of console log text plus pagination metadata.
#[derive(Debug, Clone, Default)]
pub struct ConsoleChunk {
    pub text: String,
    /// Byte offset of the end of this chunk (for incremental reads).
    pub next_offset: u64,
    /// Whether more data is still being produced by a running build.
    pub has_more: bool,
}

/// Minimal build state used by bounded agent-session polling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildState {
    pub build_number: u64,
    pub building: bool,
    pub result: Option<String>,
    pub timestamp: u64,
    pub duration: u64,
}

/// Queue lifecycle state returned by the queue-item endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueTrackingState {
    pub queue_id: i64,
    pub cancelled: bool,
    pub executable_number: Option<u64>,
    pub why: String,
    pub blocked: bool,
    pub buildable: bool,
    pub stuck: bool,
    pub missing: bool,
}

// ─── Pipeline overview (wfapi) ────────────────────────────────────────────

/// A single stage in a Workflow / Declarative Pipeline run.
///
/// Populated from the `wfapi/describe` endpoint provided by the Jenkins
/// `pipeline-rest-api` plugin. When the plugin isn't installed, callers get
/// an error and should gracefully fall back to the plain build view.
#[derive(Debug, Clone, Default)]
pub struct PipelineStage {
    pub id: String,
    pub name: String,
    /// One of `SUCCESS`, `FAILED`, `UNSTABLE`, `ABORTED`, `IN_PROGRESS`,
    /// `PAUSED_PENDING_INPUT`, `NOT_EXECUTED`, `QUEUED`.
    pub status: String,
    /// Stage start time in ms since epoch.
    pub start_time_millis: u64,
    /// Stage duration in ms.
    pub duration_millis: u64,
    /// Nested parallel branches, if any.
    pub parallel: Vec<PipelineStage>,
}

impl PipelineStage {
    /// Is this stage currently executing (or paused)?
    pub fn is_running(&self) -> bool {
        matches!(
            self.status.as_str(),
            "IN_PROGRESS" | "PAUSED_PENDING_INPUT" | "QUEUED"
        )
    }
}

/// A Pipeline run overview: metadata + all its top-level stages.
#[derive(Debug, Clone, Default)]
pub struct PipelineRun {
    pub job_full_name: String,
    pub build_number: u64,
    /// Overall run status (same set as `PipelineStage::status`).
    pub status: String,
    pub start_time_millis: u64,
    pub duration_millis: u64,
    pub stages: Vec<PipelineStage>,
}

// ─── Activity (queue + executors) ─────────────────────────────────────────

/// A build currently assigned to an executor slot.
#[derive(Debug, Clone, Default)]
pub struct RunningBuild {
    /// Short job display name (what Jenkins shows in the header).
    pub job_name: String,
    /// Full Jenkins job path (`folder/sub/job`), when derivable from the URL.
    pub job_full_name: Option<String>,
    pub build_number: Option<u64>,
    pub url: String,
    /// The node (master / agent) running the build.
    pub node: String,
    /// Zero-based executor index on the node.
    pub executor: u32,
    /// Build start time in ms since epoch.
    pub timestamp: u64,
    /// Duration so far in ms (`0` if Jenkins did not report one).
    pub elapsed_millis: u64,
    /// Best-effort progress (0-100) as reported by Jenkins, if any.
    pub progress: Option<u32>,
}

/// A single queue item (pending build waiting for an executor).
#[derive(Debug, Clone, Default)]
pub struct QueueItem {
    pub id: i64,
    pub job_name: String,
    pub job_full_name: Option<String>,
    /// Jenkins' `why` field explains what is blocking the item.
    pub why: String,
    /// Time (ms since epoch) the item was enqueued.
    pub in_queue_since: u64,
    pub blocked: bool,
    pub buildable: bool,
    pub stuck: bool,
}

/// Combined activity snapshot: what's running, what's queued.
#[derive(Debug, Clone, Default)]
pub struct Activity {
    pub running: Vec<RunningBuild>,
    pub queue: Vec<QueueItem>,
}
