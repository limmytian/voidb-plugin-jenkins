//! Minimal Jenkins REST API client.
//!
//! Author: Limmy

use std::time::Duration;

use anyhow::{Result, anyhow};
use reqwest::Client;
use serde_json::Value;

use crate::config::{JenkinsAuth, JenkinsConfig};
use crate::types::{
    Activity, BuildState, BuildSummary, ConsoleChunk, JobDetail, JobSummary, PipelineRun,
    PipelineStage, QueueItem, QueueTrackingState, RunningBuild,
};

/// Create a configured `reqwest::Client` honoring timeout + TLS settings.
pub fn create_client(config: &JenkinsConfig) -> Result<Client> {
    let builder = Client::builder()
        .timeout(Duration::from_secs(config.timeout.max(1)))
        .danger_accept_invalid_certs(!config.verify_ssl)
        .user_agent("voidb-jenkins/0.1");
    builder
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {}", e))
}

/// Apply auth to a request builder.
fn apply_auth(req: reqwest::RequestBuilder, config: &JenkinsConfig) -> reqwest::RequestBuilder {
    match &config.auth {
        JenkinsAuth::Basic { username, token } => req.basic_auth(username, Some(token)),
        JenkinsAuth::None => req,
    }
}

/// Join the base URL with a relative path.
fn url(config: &JenkinsConfig, path: &str) -> String {
    let base = config.base_url();
    if path.starts_with('/') {
        format!("{}{}", base, path)
    } else {
        format!("{}/{}", base, path)
    }
}

/// Reject potentially unsafe path segments.
fn sanitize_segment(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(anyhow!("path segment must not be empty"));
    }
    if s.contains('/') || s.contains("..") || s.contains('\0') || s.contains('\\') {
        return Err(anyhow!("invalid path segment: {}", s));
    }
    Ok(())
}

/// Convert a dotted job full-name (e.g. `folder/subfolder/job`) into the
/// Jenkins path form `job/folder/job/subfolder/job/job`.
pub fn job_path_segments(full_name: &str) -> Result<String> {
    let mut parts = Vec::new();
    for seg in full_name.split('/') {
        if seg.is_empty() {
            continue;
        }
        sanitize_segment(seg)?;
        parts.push(format!("job/{}", seg));
    }
    if parts.is_empty() {
        return Err(anyhow!("empty job full-name"));
    }
    Ok(parts.join("/"))
}

/// Ping the Jenkins API (`/api/json?tree=nodeName`).
pub async fn ping(client: &Client, config: &JenkinsConfig) -> Result<String> {
    let u = url(config, "/api/json?tree=nodeName,nodeDescription,url");
    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow!("reading body failed: {}", e))?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;
    let node = v
        .get("nodeName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(node)
}

/// List jobs under a folder path (`""` means the root).
///
/// `folder_full_name` uses `/`-delimited Jenkins full names
/// (e.g. `"team-a/deployments"`).
pub async fn list_jobs(
    client: &Client,
    config: &JenkinsConfig,
    folder_full_name: &str,
) -> Result<Vec<JobSummary>> {
    let base = if folder_full_name.is_empty() {
        String::new()
    } else {
        format!("/{}", job_path_segments(folder_full_name)?)
    };
    let u = url(
        config,
        &format!("{}/api/json?tree=jobs[name,url,color,_class]", base),
    );

    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }

    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;
    let jobs = v
        .get("jobs")
        .and_then(|j| j.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(jobs.len());
    for j in jobs {
        let js: JobSummary =
            serde_json::from_value(j).map_err(|e| anyhow!("job parse error: {}", e))?;
        out.push(js);
    }
    Ok(out)
}

/// Fetch job detail + most recent builds (up to `max_builds`).
pub async fn get_job_detail(
    client: &Client,
    config: &JenkinsConfig,
    job_full_name: &str,
    max_builds: usize,
) -> Result<JobDetail> {
    let path = job_path_segments(job_full_name)?;
    let tree = format!(
        "name,fullName,description,url,buildable,inQueue,builds[number,url,result,timestamp,duration,building]{{0,{}}}",
        max_builds
    );
    let u = url(config, &format!("/{}/api/json?tree={}", path, tree));

    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }

    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;

    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let full_name = v
        .get("fullName")
        .and_then(|x| x.as_str())
        .unwrap_or(&name)
        .to_string();
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let url_s = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let buildable = v
        .get("buildable")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let in_queue = v.get("inQueue").and_then(|x| x.as_bool()).unwrap_or(false);
    let builds = v
        .get("builds")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let mut b_out: Vec<BuildSummary> = Vec::with_capacity(builds.len());
    for b in builds {
        if let Ok(bs) = serde_json::from_value::<BuildSummary>(b) {
            b_out.push(bs);
        }
    }

    Ok(JobDetail {
        name,
        full_name,
        description,
        url: url_s,
        buildable,
        in_queue,
        builds: b_out,
    })
}

/// Ask Jenkins for a CSRF crumb. Returns `Some((header, value))` when CSRF
/// protection is enabled, otherwise `None` (endpoint returns 404).
async fn get_crumb(client: &Client, config: &JenkinsConfig) -> Result<Option<(String, String)>> {
    let u = url(config, "/crumbIssuer/api/json");
    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        // Some Jenkins deployments disable the crumb issuer; treat as absent.
        return Ok(None);
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("crumb parse: {}", e))?;
    let field = v
        .get("crumbRequestField")
        .and_then(|x| x.as_str())
        .unwrap_or("Jenkins-Crumb")
        .to_string();
    let crumb = v
        .get("crumb")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if crumb.is_empty() {
        Ok(None)
    } else {
        Ok(Some((field, crumb)))
    }
}

/// Trigger a build for a job. Returns the `Location` header of the queue item
/// if Jenkins provided one.
pub async fn trigger_build(
    client: &Client,
    config: &JenkinsConfig,
    job_full_name: &str,
) -> Result<Option<String>> {
    let path = job_path_segments(job_full_name)?;
    let u = url(config, &format!("/{}/build", path));

    let mut req = apply_auth(client.post(&u), config);
    if let Some((field, crumb)) = get_crumb(client, config).await? {
        req = req.header(field, crumb);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    if !status.is_success() && status.as_u16() != 201 {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {}: {}", status, truncate(&body, 200)));
    }
    Ok(location)
}

/// Abort a running build.
pub async fn abort_build(
    client: &Client,
    config: &JenkinsConfig,
    job_full_name: &str,
    build_number: u64,
) -> Result<()> {
    let path = job_path_segments(job_full_name)?;
    let u = url(config, &format!("/{}/{}/stop", path, build_number));

    let mut req = apply_auth(client.post(&u), config);
    if let Some((field, crumb)) = get_crumb(client, config).await? {
        req = req.header(field, crumb);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    if !status.is_success() && status.as_u16() != 302 {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {}: {}", status, truncate(&body, 200)));
    }
    Ok(())
}

/// Fetch a chunk of console log starting at `start` bytes.
///
/// Uses the `/progressiveText` endpoint so we can stream logs of running
/// builds incrementally.
pub async fn fetch_console(
    client: &Client,
    config: &JenkinsConfig,
    job_full_name: &str,
    build_number: u64,
    start: u64,
) -> Result<ConsoleChunk> {
    let path = job_path_segments(job_full_name)?;
    let u = url(
        config,
        &format!(
            "/{}/{}/logText/progressiveText?start={}",
            path, build_number, start
        ),
    );

    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("HTTP {}: {}", status, truncate(&body, 200)));
    }

    let has_more = resp
        .headers()
        .get("x-more-data")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let next_offset = resp
        .headers()
        .get("x-text-size")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(start);

    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;

    Ok(ConsoleChunk {
        text,
        next_offset,
        has_more,
    })
}

/// Fetch only the stable build fields needed by a live wait session.
pub async fn fetch_build_state(
    client: &Client,
    config: &JenkinsConfig,
    job_full_name: &str,
    build_number: u64,
) -> Result<BuildState> {
    let path = job_path_segments(job_full_name)?;
    let u = url(
        config,
        &format!(
            "/{}/{}/api/json?tree=number,result,timestamp,duration,building",
            path, build_number
        ),
    );
    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;
    Ok(BuildState {
        build_number: value
            .get("number")
            .and_then(Value::as_u64)
            .unwrap_or(build_number),
        building: value
            .get("building")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        result: value
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_string),
        timestamp: value.get("timestamp").and_then(Value::as_u64).unwrap_or(0),
        duration: value.get("duration").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Fetch a single queue item, including its queue-to-build transition.
pub async fn fetch_queue_tracking_state(
    client: &Client,
    config: &JenkinsConfig,
    queue_id: i64,
) -> Result<QueueTrackingState> {
    let u = url(
        config,
        &format!(
            "/queue/item/{queue_id}/api/json?tree=id,cancelled,why,blocked,buildable,stuck,executable[number]"
        ),
    );
    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(QueueTrackingState {
            queue_id,
            missing: true,
            ..QueueTrackingState::default()
        });
    }
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;
    Ok(QueueTrackingState {
        queue_id: value.get("id").and_then(Value::as_i64).unwrap_or(queue_id),
        cancelled: value
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        executable_number: value
            .get("executable")
            .and_then(|executable| executable.get("number"))
            .and_then(Value::as_u64),
        why: value
            .get("why")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        blocked: value
            .get("blocked")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        buildable: value
            .get("buildable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        stuck: value.get("stuck").and_then(Value::as_bool).unwrap_or(false),
        missing: false,
    })
}

// ─── Pipeline (wfapi) ─────────────────────────────────────────────────────

/// Fetch the Pipeline stages for a build via the `pipeline-rest-api` plugin.
///
/// Returns a `PipelineRun`. If the build isn't a Pipeline job, or if the
/// plugin isn't installed, Jenkins responds with 404 and we surface that as
/// an error — the caller should fall back to the plain build view.
pub async fn get_pipeline_run(
    client: &Client,
    config: &JenkinsConfig,
    job_full_name: &str,
    build_number: u64,
) -> Result<PipelineRun> {
    let path = job_path_segments(job_full_name)?;
    let u = url(
        config,
        &format!("/{}/{}/wfapi/describe", path, build_number),
    );

    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if status.as_u16() == 404 {
        return Err(anyhow!(
            "Pipeline API not available (is pipeline-rest-api plugin installed?)"
        ));
    }
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }

    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;

    let stages = v
        .get("stages")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let stages = stages
        .into_iter()
        .map(parse_pipeline_stage)
        .collect::<Vec<_>>();

    Ok(PipelineRun {
        job_full_name: job_full_name.to_string(),
        build_number,
        status: v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        start_time_millis: v
            .get("startTimeMillis")
            .and_then(|s| s.as_u64())
            .unwrap_or(0),
        duration_millis: v
            .get("durationMillis")
            .and_then(|s| s.as_u64())
            .unwrap_or(0),
        stages,
    })
}

fn parse_pipeline_stage(v: Value) -> PipelineStage {
    PipelineStage {
        id: v
            .get("id")
            .and_then(|s| {
                s.as_str()
                    .map(str::to_string)
                    .or_else(|| s.as_u64().map(|n| n.to_string()))
            })
            .unwrap_or_default(),
        name: v
            .get("name")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        status: v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        start_time_millis: v
            .get("startTimeMillis")
            .and_then(|s| s.as_u64())
            .unwrap_or(0),
        duration_millis: v
            .get("durationMillis")
            .and_then(|s| s.as_u64())
            .unwrap_or(0),
        parallel: Vec::new(),
    }
}

// ─── Activity (queue + running) ───────────────────────────────────────────

/// Fetch a snapshot of currently-running builds (from `/computer`) plus all
/// queue items (from `/queue`).
pub async fn get_activity(client: &Client, config: &JenkinsConfig) -> Result<Activity> {
    let running = fetch_running(client, config).await?;
    let queue = fetch_queue(client, config).await?;
    Ok(Activity { running, queue })
}

async fn fetch_running(client: &Client, config: &JenkinsConfig) -> Result<Vec<RunningBuild>> {
    let u = url(
        config,
        "/computer/api/json?tree=computer[displayName,executors[idle,progress,currentExecutable[number,url,fullDisplayName,timestamp,estimatedDuration]],oneOffExecutors[idle,progress,currentExecutable[number,url,fullDisplayName,timestamp,estimatedDuration]]]",
    );

    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }

    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;

    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;

    let mut out = Vec::new();
    let computers = v
        .get("computer")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    for computer in computers {
        let node = computer
            .get("displayName")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();

        for key in ["executors", "oneOffExecutors"] {
            let execs = computer
                .get(key)
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();

            for (idx, exec) in execs.into_iter().enumerate() {
                let exe = match exec.get("currentExecutable") {
                    Some(Value::Object(_)) => exec.get("currentExecutable").unwrap(),
                    _ => continue,
                };

                let full_display = exe
                    .get("fullDisplayName")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let url = exe
                    .get("url")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let number = exe.get("number").and_then(|x| x.as_u64());
                let timestamp = exe.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
                let elapsed = if timestamp > 0 {
                    now_ms.saturating_sub(timestamp)
                } else {
                    0
                };
                let progress = exec
                    .get("progress")
                    .and_then(|p| p.as_i64())
                    .filter(|p| *p >= 0)
                    .map(|p| p as u32);

                let (job_name, job_full_name) = split_full_display(&full_display, &url);

                out.push(RunningBuild {
                    job_name,
                    job_full_name,
                    build_number: number,
                    url,
                    node: node.clone(),
                    executor: idx as u32,
                    timestamp,
                    elapsed_millis: elapsed,
                    progress,
                });
            }
        }
    }

    Ok(out)
}

/// Try to pull the "short" job name and its full-name from a Jenkins
/// `fullDisplayName` (e.g. `folder » sub » job #42`) plus URL path.
fn split_full_display(full_display: &str, url: &str) -> (String, Option<String>) {
    // Remove trailing " #NNN" build number part, if present.
    let without_build = match full_display.rsplit_once(" #") {
        Some((head, tail)) if tail.chars().all(|c| c.is_ascii_digit()) => head.to_string(),
        _ => full_display.to_string(),
    };

    // Short job name is the last segment of the display name.
    let short = without_build
        .rsplit(" » ")
        .next()
        .unwrap_or(&without_build)
        .to_string();

    // Derive the Jenkins full-name (slash-joined) from the URL path.
    // URLs look like: .../job/folder/job/sub/job/name/42/  or  .../job/name/42/
    //
    // We walk the path after the first `/job/` prefix and collect the names
    // that follow each subsequent `job` marker, so we correctly handle jobs
    // actually named `job`.
    let full = if let Some(rest) = url.find("/job/") {
        let tail = &url[rest + 1..]; // keep leading "job/..." segment
        let segments: Vec<&str> = tail.split('/').filter(|s| !s.is_empty()).collect();
        let mut names = Vec::new();
        let mut i = 0;
        while i + 1 < segments.len() {
            if segments[i] == "job" {
                names.push(segments[i + 1]);
                i += 2;
            } else {
                i += 1;
            }
        }
        if names.is_empty() {
            None
        } else {
            Some(names.join("/"))
        }
    } else {
        None
    };

    (short, full)
}

async fn fetch_queue(client: &Client, config: &JenkinsConfig) -> Result<Vec<QueueItem>> {
    let u = url(
        config,
        "/queue/api/json?tree=items[id,task[name,url,fullDisplayName],why,inQueueSince,blocked,buildable,stuck]",
    );

    let resp = apply_auth(client.get(&u), config)
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| anyhow!("body: {}", e))?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, truncate(&text, 200)));
    }

    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("parse error: {}: {}", e, truncate(&text, 200)))?;

    let items = v
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let task_url = item
            .get("task")
            .and_then(|t| t.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();
        let task_name = item
            .get("task")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let task_display = item
            .get("task")
            .and_then(|t| t.get("fullDisplayName"))
            .and_then(|n| n.as_str())
            .unwrap_or(&task_name)
            .to_string();

        let (short, full) = split_full_display(&task_display, &task_url);
        out.push(QueueItem {
            id: item.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
            job_name: if short.is_empty() { task_name } else { short },
            job_full_name: full,
            why: item
                .get("why")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            in_queue_since: item
                .get("inQueueSince")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            blocked: item
                .get("blocked")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            buildable: item
                .get("buildable")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            stuck: item.get("stuck").and_then(|x| x.as_bool()).unwrap_or(false),
        });
    }

    Ok(out)
}

/// Cancel a queue item (before it starts building).
pub async fn cancel_queue_item(
    client: &Client,
    config: &JenkinsConfig,
    queue_id: i64,
) -> Result<()> {
    let u = url(config, &format!("/queue/cancelItem?id={}", queue_id));

    let mut req = apply_auth(client.post(&u), config);
    if let Some((field, crumb)) = get_crumb(client, config).await? {
        req = req.header(field, crumb);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| anyhow!("request failed: {}", e))?;
    let status = resp.status();
    // Jenkins redirects to the queue page on success (302); also tolerate 2xx.
    if status.is_success() || status.as_u16() == 302 || status.as_u16() == 204 {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("HTTP {}: {}", status, truncate(&body, 200)))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_full_display_root_job() {
        let (short, full) =
            split_full_display("my-job #42", "https://ci.example.com/job/my-job/42/");
        assert_eq!(short, "my-job");
        assert_eq!(full, Some("my-job".to_string()));
    }

    #[test]
    fn split_full_display_nested_folders() {
        let (short, full) = split_full_display(
            "team » service » build #7",
            "https://ci.example.com/job/team/job/service/job/build/7/",
        );
        assert_eq!(short, "build");
        assert_eq!(full, Some("team/service/build".to_string()));
    }

    #[test]
    fn split_full_display_no_build_number() {
        let (short, full) =
            split_full_display("folder » job", "https://ci.example.com/job/folder/job/job/");
        assert_eq!(short, "job");
        assert_eq!(full, Some("folder/job".to_string()));
    }

    #[test]
    fn split_full_display_empty_url() {
        let (short, full) = split_full_display("some » job #1", "");
        assert_eq!(short, "job");
        assert!(full.is_none());
    }

    #[test]
    fn job_path_segments_single() {
        assert_eq!(job_path_segments("foo").unwrap(), "job/foo");
    }

    #[test]
    fn job_path_segments_nested() {
        assert_eq!(job_path_segments("a/b/c").unwrap(), "job/a/job/b/job/c");
    }

    #[test]
    fn job_path_segments_rejects_traversal() {
        assert!(job_path_segments("a/../b").is_err());
        assert!(job_path_segments("").is_err());
    }
}
