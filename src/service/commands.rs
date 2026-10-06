//! Commands sent from the UI to the Jenkins service.
//!
//! Author: Limmy

/// Top-level command enum for the Jenkins service.
pub enum JenkinsCommand {
    /// Verify connectivity; the service replies with `JenkinsEvent::Connected`
    /// or `JenkinsEvent::Error`.
    Ping,

    /// List the jobs inside the given folder (use `""` for the root).
    ListJobs { folder: String },

    /// Fetch detailed info + recent builds for a job.
    GetJobDetail { job_full_name: String },

    /// Trigger a build for a job.
    TriggerBuild { job_full_name: String },

    /// Abort a running build.
    AbortBuild {
        job_full_name: String,
        build_number: u64,
    },

    /// Fetch (possibly incremental) console output for a build.
    FetchConsole {
        job_full_name: String,
        build_number: u64,
        start: u64,
    },

    /// Fetch Pipeline stages (wfapi) for a build.
    GetPipelineRun {
        job_full_name: String,
        build_number: u64,
    },

    /// Snapshot currently-running and queued builds.
    GetActivity,

    /// Cancel a queue item.
    CancelQueueItem { queue_id: i64 },

    /// Stop the background task.
    Disconnect,
}
