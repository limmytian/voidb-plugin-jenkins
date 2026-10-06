//! Events emitted by the Jenkins service back to the UI.
//!
//! Author: Limmy

use crate::types::{Activity, ConsoleChunk, JobDetail, JobSummary, PipelineRun};

/// Top-level event enum for the Jenkins service.
pub enum JenkinsEvent {
    /// Connection to Jenkins established. `node_name` is the master label (may be empty).
    Connected { node_name: String },

    /// The jobs under `folder` have been listed.
    JobsListed {
        folder: String,
        jobs: Vec<JobSummary>,
    },

    /// A job's detail (and recent builds) has been fetched.
    JobDetailLoaded { detail: JobDetail },

    /// A new build was triggered; `queue_url` is the queue item URL when available.
    BuildTriggered {
        job_full_name: String,
        queue_url: Option<String>,
    },

    /// A build was aborted.
    BuildAborted {
        job_full_name: String,
        build_number: u64,
    },

    /// A chunk of console log was fetched.
    ConsoleFetched {
        job_full_name: String,
        build_number: u64,
        chunk: ConsoleChunk,
    },

    /// Pipeline stages for a build have been fetched.
    PipelineLoaded { run: PipelineRun },

    /// A running-builds + queue snapshot has been fetched.
    ActivityLoaded { activity: Activity },

    /// A queue item was cancelled.
    QueueItemCancelled { queue_id: i64 },

    /// Any operation failure; TUI displays this as an error banner.
    Error(String),
}
