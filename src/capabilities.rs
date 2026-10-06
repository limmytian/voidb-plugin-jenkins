#![allow(clippy::result_large_err)]

use serde_json::{Value, json};
use voidb_core::{
    CapabilityDefinition, CapabilityError, CapabilityErrorCategory, CapabilityInvocation,
    CapabilityInvocationResult, CapabilityRiskLevel, CredentialClass, InvocationOutputPage,
    InvocationStatus, Pagination, RedactionStatus, TargetSystemFailure, audit_json_summary,
};

use crate::agent_session::{
    BUILD_WAIT_CAPABILITY, CONSOLE_FOLLOW_CAPABILITY, QUEUE_WATCH_CAPABILITY,
    jenkins_live_session_contract,
};
use crate::config::{JenkinsAuth, JenkinsConfig};
use crate::service::JenkinsService;
use crate::types::{
    Activity, BuildSummary, JobDetail, JobSummary, PipelineRun, PipelineStage, QueueItem,
    RunningBuild,
};

const PLUGIN_ID: &str = "jenkins";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_PAGE_LIMIT: usize = 50;
const MAX_PAGE_LIMIT: usize = 200;
const DEFAULT_CONSOLE_LIMIT_BYTES: usize = 64 * 1024;
const MAX_CONSOLE_LIMIT_BYTES: usize = 512 * 1024;

pub fn jenkins_capabilities() -> Vec<CapabilityDefinition> {
    vec![
        capability(
            "diagnostics",
            "Return agent-safe Jenkins profile diagnostics without opening a server connection.",
            empty_input_schema(),
            json!({
                "type": "object",
                "required": [
                    "url_configured",
                    "url_scheme",
                    "auth_type",
                    "timeout_secs",
                    "verify_ssl",
                    "network_checked"
                ],
                "properties": {
                    "url_configured": { "type": "boolean" },
                    "url_scheme": { "type": ["string", "null"] },
                    "auth_type": { "type": "string" },
                    "timeout_secs": { "type": "integer", "minimum": 0 },
                    "verify_ssl": { "type": "boolean" },
                    "network_checked": { "type": "boolean" }
                },
                "additionalProperties": false
            }),
            vec!["connection.read", "jenkins.diagnostics"],
            false,
            false,
            false,
        ),
        capability(
            "jobs",
            "List Jenkins jobs in a folder with bounded output.",
            json!({
                "type": "object",
                "properties": {
                    "folder": { "type": "string" }
                },
                "additionalProperties": false
            }),
            list_schema("jobs", job_schema()),
            vec!["connection.read", "jenkins.jobs.list"],
            false,
            false,
            false,
        ),
        capability(
            "job_detail",
            "Fetch Jenkins job metadata and recent builds with bounded build output.",
            json!({
                "type": "object",
                "required": ["job_full_name"],
                "properties": {
                    "job_full_name": { "type": "string", "minLength": 1 }
                },
                "additionalProperties": false
            }),
            job_detail_schema(),
            vec!["connection.read", "jenkins.jobs.detail"],
            false,
            false,
            false,
        ),
        capability(
            "activity",
            "Fetch a bounded Jenkins running-build and queue snapshot.",
            empty_input_schema(),
            activity_schema(),
            vec!["connection.read", "jenkins.activity"],
            false,
            false,
            false,
        ),
        capability(
            "console",
            "Fetch bounded Jenkins console output for one build.",
            json!({
                "type": "object",
                "required": ["job_full_name", "build_number"],
                "properties": {
                    "job_full_name": { "type": "string", "minLength": 1 },
                    "build_number": { "type": "integer", "minimum": 0 },
                    "start": { "type": "integer", "minimum": 0 },
                    "max_bytes": console_limit_schema()
                },
                "additionalProperties": false
            }),
            console_schema(),
            vec!["connection.read", "jenkins.console"],
            false,
            false,
            false,
        ),
        live_capability(
            CONSOLE_FOLLOW_CAPABILITY,
            "Follow Jenkins progressive console output with exact byte-offset continuation.",
            vec!["connection.read", "jenkins.console"],
        ),
        live_capability(
            BUILD_WAIT_CAPABILITY,
            "Wait for a Jenkins build to reach a terminal result.",
            vec!["connection.read", "jenkins.builds.wait"],
        ),
        live_capability(
            QUEUE_WATCH_CAPABILITY,
            "Track a Jenkins queue item until cancellation, disappearance, or build assignment.",
            vec!["connection.read", "jenkins.queue.track"],
        ),
        capability(
            "pipeline",
            "Fetch bounded Jenkins Pipeline stage metadata for one build.",
            json!({
                "type": "object",
                "required": ["job_full_name", "build_number"],
                "properties": {
                    "job_full_name": { "type": "string", "minLength": 1 },
                    "build_number": { "type": "integer", "minimum": 0 }
                },
                "additionalProperties": false
            }),
            list_schema("stages", pipeline_stage_schema()),
            vec!["connection.read", "jenkins.pipeline"],
            false,
            false,
            false,
        ),
        capability(
            "trigger_build",
            "Trigger a Jenkins build.",
            json!({
                "type": "object",
                "required": ["job_full_name"],
                "properties": {
                    "job_full_name": { "type": "string", "minLength": 1 }
                },
                "additionalProperties": false
            }),
            mutation_output_schema(),
            vec!["connection.write", "jenkins.builds.trigger"],
            true,
            false,
            true,
        ),
        capability(
            "abort_build",
            "Abort a running Jenkins build.",
            json!({
                "type": "object",
                "required": ["job_full_name", "build_number"],
                "properties": {
                    "job_full_name": { "type": "string", "minLength": 1 },
                    "build_number": { "type": "integer", "minimum": 0 }
                },
                "additionalProperties": false
            }),
            mutation_output_schema(),
            vec!["connection.write", "jenkins.builds.abort"],
            true,
            false,
            true,
        ),
        capability(
            "cancel_queue_item",
            "Cancel a queued Jenkins build item.",
            json!({
                "type": "object",
                "required": ["queue_id"],
                "properties": {
                    "queue_id": { "type": "integer" }
                },
                "additionalProperties": false
            }),
            mutation_output_schema(),
            vec!["connection.write", "jenkins.queue.cancel"],
            true,
            false,
            true,
        ),
    ]
}

pub async fn invoke_jenkins_capability(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    if invocation.plugin_id != PLUGIN_ID {
        return Err(validation_error(
            "validation.plugin_mismatch",
            "Invocation plugin_id does not match Jenkins.",
            json!({ "expected": PLUGIN_ID, "actual": invocation.plugin_id }),
        ));
    }

    match invocation.capability_id.as_str() {
        "diagnostics" => Ok(diagnostics_result(config, invocation.id)),
        "jobs" => invoke_jobs(config, invocation).await,
        "job_detail" => invoke_job_detail(config, invocation).await,
        "activity" => invoke_activity(config, invocation).await,
        "console" => invoke_console(config, invocation).await,
        "console_follow" | "build_wait" | "queue_watch" => Err(unavailable_error(
            "unavailable.session_required",
            "This Jenkins live workflow requires a persistent agent session.",
            json!({ "capability_id": invocation.capability_id }),
        )),
        "pipeline" => invoke_pipeline(config, invocation).await,
        "trigger_build" => invoke_trigger_build(config, invocation).await,
        "abort_build" => invoke_abort_build(config, invocation).await,
        "cancel_queue_item" => invoke_cancel_queue_item(config, invocation).await,
        other => Err(unavailable_error(
            "unavailable.capability_not_found",
            "Jenkins capability was not found.",
            json!({ "capability_id": other }),
        )),
    }
}

fn live_capability(
    qualified_id: &str,
    description: &str,
    permissions: Vec<&str>,
) -> CapabilityDefinition {
    let id = qualified_id
        .strip_prefix("jenkins.")
        .expect("Jenkins live capability ID");
    let (purpose, contract) =
        jenkins_live_session_contract(qualified_id).expect("Jenkins live contract");
    CapabilityDefinition {
        plugin_id: PLUGIN_ID.to_string(),
        id: id.to_string(),
        description: description.to_string(),
        input_schema: live_read_schema(),
        output_schema: live_batch_schema(),
        permissions: permissions.into_iter().map(str::to_string).collect(),
        authorization: jenkins_live_authorization(qualified_id, purpose.clone()),
        risk: CapabilityRiskLevel::ReadOnly,
        destructive: false,
        streaming: true,
        execution_mode: voidb_core::CapabilityExecutionMode::SessionOnly,
        session_handoff: Some(
            voidb_core::CapabilitySessionHandoff::new(purpose, [qualified_id])
                .with_live_session(contract),
        ),
        connection_required: true,
        required_secret_classes: Vec::<CredentialClass>::new(),
        supports_dry_run: false,
        default_timeout_ms: Some(DEFAULT_TIMEOUT_MS),
    }
}

fn jenkins_live_authorization(
    capability: &str,
    purpose: voidb_core::PluginSessionPurpose,
) -> voidb_core::CapabilityAuthorizationMetadata {
    let resource = |path: &str, label: &str, value_type| {
        voidb_core::CapabilityApprovalField::new(path, label, value_type).required()
    };
    let fields = match capability {
        CONSOLE_FOLLOW_CAPABILITY | BUILD_WAIT_CAPABILITY => vec![
            resource(
                "/resource/job_full_name",
                "Job",
                voidb_core::CapabilityApprovalValueType::ResourceId,
            ),
            resource(
                "/resource/build_number",
                "Build number",
                voidb_core::CapabilityApprovalValueType::Integer,
            ),
        ],
        QUEUE_WATCH_CAPABILITY => vec![resource(
            "/resource/queue_id",
            "Queue item",
            voidb_core::CapabilityApprovalValueType::Integer,
        )],
        _ => Vec::new(),
    };
    voidb_core::CapabilityAuthorizationMetadata::declared()
        .with_session_purposes(vec![purpose])
        .with_note("Jenkins live-session target identity is revalidated at session open.")
        .with_approval_schema(voidb_core::CapabilityApprovalSchema::v1(fields))
}

fn live_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "after_sequence": { "type": "integer", "minimum": 0 },
            "max_events": { "type": "integer", "minimum": 1, "maximum": 1000 },
            "max_bytes": { "type": "integer", "minimum": 1, "maximum": 1048576 }
        },
        "additionalProperties": false
    })
}

fn live_batch_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "protocol_version",
            "events",
            "next_sequence",
            "source_closed",
            "dropped_events",
            "dropped_bytes",
            "coalesced_events",
            "reconnect_attempts"
        ],
        "properties": {
            "protocol_version": { "type": "integer", "const": 1 },
            "events": { "type": "array", "maxItems": 1000 },
            "next_sequence": { "type": "integer", "minimum": 1 },
            "resume_cursor": { "type": "object" },
            "source_closed": { "type": "boolean" },
            "dropped_events": { "type": "integer", "minimum": 0 },
            "dropped_bytes": { "type": "integer", "minimum": 0 },
            "coalesced_events": { "type": "integer", "minimum": 0 },
            "reconnect_attempts": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": false
    })
}

#[allow(clippy::too_many_arguments)]
fn capability(
    id: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    permissions: Vec<&str>,
    destructive: bool,
    streaming: bool,
    supports_dry_run: bool,
) -> CapabilityDefinition {
    CapabilityDefinition {
        plugin_id: PLUGIN_ID.to_string(),
        id: id.to_string(),
        description: description.to_string(),
        input_schema,
        output_schema,
        permissions: permissions.into_iter().map(str::to_string).collect(),
        authorization: jenkins_authorization_metadata(id),
        risk: CapabilityRiskLevel::from_destructive(destructive),
        destructive,
        streaming,
        execution_mode: voidb_core::CapabilityExecutionMode::Stateless,
        session_handoff: None,
        connection_required: true,
        required_secret_classes: Vec::<CredentialClass>::new(),
        supports_dry_run,
        default_timeout_ms: Some(DEFAULT_TIMEOUT_MS),
    }
}

fn jenkins_authorization_metadata(id: &str) -> voidb_core::CapabilityAuthorizationMetadata {
    let job = || {
        voidb_core::CapabilityApprovalField::new(
            "/job_full_name",
            "Job",
            voidb_core::CapabilityApprovalValueType::ResourceId,
        )
        .required()
    };
    let fields = match id {
        "jobs" => vec![
            voidb_core::CapabilityApprovalField::new(
                "/folder",
                "Job folder",
                voidb_core::CapabilityApprovalValueType::Path,
            )
            .with_constraint(voidb_core::CapabilityConstraintKind::Prefix),
        ],
        "job_detail" | "trigger_build" => vec![job()],
        "console" | "pipeline" | "abort_build" => vec![
            job(),
            voidb_core::CapabilityApprovalField::new(
                "/build_number",
                "Build number",
                voidb_core::CapabilityApprovalValueType::Integer,
            )
            .required(),
        ],
        "cancel_queue_item" => vec![
            voidb_core::CapabilityApprovalField::new(
                "/queue_id",
                "Queue item",
                voidb_core::CapabilityApprovalValueType::Integer,
            )
            .required(),
        ],
        _ => Vec::new(),
    };
    let metadata = voidb_core::CapabilityAuthorizationMetadata::declared();
    if fields.is_empty() {
        metadata
    } else {
        metadata
            .with_note(if id == "console" {
                "Job and build are revalidated; progressive console sessions remain deferred."
            } else {
                "Job, build, or queue identity is revalidated before execution."
            })
            .with_approval_schema(voidb_core::CapabilityApprovalSchema::v1(fields))
    }
}

fn diagnostics_result(config: &JenkinsConfig, invocation_id: String) -> CapabilityInvocationResult {
    let output = json!({
        "url_configured": !config.url.trim().is_empty(),
        "url_scheme": url_scheme(&config.url),
        "auth_type": auth_type(config),
        "timeout_secs": config.timeout,
        "verify_ssl": config.verify_ssl,
        "network_checked": false,
    });
    result(invocation_id, output.clone(), output, None)
}

async fn invoke_jobs(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let folder = optional_string(&invocation.input, "folder")?.unwrap_or_default();
    let page = page_request(invocation.controls.page.as_ref())?;
    let service = service(config)?;
    let mut jobs = service
        .list_jobs(&folder)
        .await
        .map_err(|error| target_error(config, "jenkins.jobs_failed", error.to_string()))?;
    jobs.sort_by(|left, right| left.name.cmp(&right.name));
    let items = jobs.iter().map(job_json).collect::<Vec<_>>();
    Ok(paged_result(
        invocation.id,
        "jobs",
        items,
        page,
        json!({ "folder": folder }),
    ))
}

async fn invoke_job_detail(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let job_full_name = required_string(&invocation.input, "job_full_name")?;
    let page = page_request(invocation.controls.page.as_ref())?;
    let service = service(config)?;
    let detail = service
        .get_job_detail(&job_full_name)
        .await
        .map_err(|error| target_error(config, "jenkins.job_detail_failed", error.to_string()))?;
    Ok(job_detail_result(invocation.id, detail, page))
}

async fn invoke_activity(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let page = page_request(invocation.controls.page.as_ref())?;
    let service = service(config)?;
    let activity = service
        .get_activity()
        .await
        .map_err(|error| target_error(config, "jenkins.activity_failed", error.to_string()))?;
    Ok(activity_result(invocation.id, activity, page))
}

async fn invoke_console(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let job_full_name = required_string(&invocation.input, "job_full_name")?;
    let build_number = required_u64(&invocation.input, "build_number")?;
    let start = optional_u64(&invocation.input, "start")?
        .or_else(|| {
            invocation
                .controls
                .page
                .as_ref()
                .and_then(|page| page.cursor.as_deref())
                .and_then(|cursor| cursor.parse::<u64>().ok())
        })
        .unwrap_or(0);
    let max_bytes = requested_usize(
        &invocation.input,
        "max_bytes",
        DEFAULT_CONSOLE_LIMIT_BYTES,
        1,
        MAX_CONSOLE_LIMIT_BYTES,
    )?;
    let service = service(config)?;
    let chunk = service
        .fetch_console(&job_full_name, build_number, start)
        .await
        .map_err(|error| target_error(config, "jenkins.console_failed", error.to_string()))?;
    let source_bytes = chunk.text.len();
    let (text, truncated) = bounded_text(&chunk.text, max_bytes);
    let bytes_returned = text.len();
    let next_offset = if truncated {
        start.saturating_add(bytes_returned as u64)
    } else {
        chunk.next_offset
    };
    let has_more = chunk.has_more || truncated;
    let output = json!({
        "job_full_name": job_full_name,
        "build_number": build_number,
        "start": start,
        "text": text,
        "bytes_returned": bytes_returned,
        "source_bytes": source_bytes,
        "byte_limit": max_bytes,
        "truncated": truncated,
        "next_offset": next_offset,
        "has_more": has_more,
    });
    let summary = json!({
        "job_full_name": output["job_full_name"],
        "build_number": build_number,
        "bytes_returned": bytes_returned,
        "source_bytes": source_bytes,
        "truncated": truncated,
        "next_offset": next_offset,
        "has_more": has_more,
    });
    let output_page = has_more.then(|| InvocationOutputPage {
        next_cursor: Some(next_offset.to_string()),
    });
    Ok(result(invocation.id, output, summary, output_page))
}

async fn invoke_pipeline(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let job_full_name = required_string(&invocation.input, "job_full_name")?;
    let build_number = required_u64(&invocation.input, "build_number")?;
    let page = page_request(invocation.controls.page.as_ref())?;
    let service = service(config)?;
    let run = service
        .get_pipeline_run(&job_full_name, build_number)
        .await
        .map_err(|error| target_error(config, "jenkins.pipeline_failed", error.to_string()))?;
    Ok(pipeline_result(invocation.id, run, page))
}

async fn invoke_trigger_build(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let job_full_name = required_string(&invocation.input, "job_full_name")?;
    let details = json!({ "job_full_name": job_full_name });
    if invocation.controls.dry_run {
        return Ok(dry_run_result(invocation.id, "trigger_build", details));
    }

    let service = service(config)?;
    let queue_url = service
        .trigger_build(&job_full_name)
        .await
        .map_err(|error| target_error(config, "jenkins.trigger_failed", error.to_string()))?;
    let mut details = details;
    details["queue_location_present"] = json!(queue_url.is_some());
    details["queue_url_omitted"] = json!(queue_url.is_some());
    Ok(mutation_result(invocation.id, "trigger_build", details))
}

async fn invoke_abort_build(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let job_full_name = required_string(&invocation.input, "job_full_name")?;
    let build_number = required_u64(&invocation.input, "build_number")?;
    let details = json!({
        "job_full_name": job_full_name,
        "build_number": build_number,
    });
    if invocation.controls.dry_run {
        return Ok(dry_run_result(invocation.id, "abort_build", details));
    }

    let service = service(config)?;
    service
        .abort_build(&job_full_name, build_number)
        .await
        .map_err(|error| target_error(config, "jenkins.abort_failed", error.to_string()))?;
    Ok(mutation_result(invocation.id, "abort_build", details))
}

async fn invoke_cancel_queue_item(
    config: &JenkinsConfig,
    invocation: CapabilityInvocation,
) -> Result<CapabilityInvocationResult, CapabilityError> {
    let queue_id = required_i64(&invocation.input, "queue_id")?;
    let details = json!({ "queue_id": queue_id });
    if invocation.controls.dry_run {
        return Ok(dry_run_result(invocation.id, "cancel_queue_item", details));
    }

    let service = service(config)?;
    service
        .cancel_queue_item(queue_id)
        .await
        .map_err(|error| target_error(config, "jenkins.cancel_queue_failed", error.to_string()))?;
    Ok(mutation_result(invocation.id, "cancel_queue_item", details))
}

fn service(config: &JenkinsConfig) -> Result<JenkinsService, CapabilityError> {
    JenkinsService::new_direct(config)
        .map_err(|error| target_error(config, "jenkins.connect_failed", error.to_string()))
}

fn paged_result(
    invocation_id: String,
    item_key: &str,
    items: Vec<Value>,
    page: PageRequest,
    metadata: Value,
) -> CapabilityInvocationResult {
    let source_count = items.len();
    let end = page.offset.saturating_add(page.limit).min(source_count);
    let page_items = if page.offset >= source_count {
        Vec::new()
    } else {
        items[page.offset..end].to_vec()
    };
    let next_cursor = (end < source_count).then(|| end.to_string());
    let item_count = page_items.len();
    let truncated = next_cursor.is_some();
    let output = json!({
        item_key: page_items,
        "item_count": item_count,
        "source_item_count": source_count,
        "limit": page.limit,
        "cursor": page.cursor,
        "next_cursor": next_cursor,
        "truncated": truncated,
        "metadata": metadata,
    });
    let summary = json!({
        "item_key": item_key,
        "item_count": item_count,
        "source_item_count": source_count,
        "truncated": truncated,
        "next_cursor": output["next_cursor"],
    });
    let output_page = truncated.then(|| InvocationOutputPage {
        next_cursor: output["next_cursor"].as_str().map(str::to_string),
    });
    result(invocation_id, output, summary, output_page)
}

fn job_detail_result(
    invocation_id: String,
    detail: JobDetail,
    page: PageRequest,
) -> CapabilityInvocationResult {
    let builds = detail.builds.iter().map(build_json).collect::<Vec<_>>();
    let builds_source_count = builds.len();
    let end = page
        .offset
        .saturating_add(page.limit)
        .min(builds_source_count);
    let page_builds = if page.offset >= builds_source_count {
        Vec::new()
    } else {
        builds[page.offset..end].to_vec()
    };
    let next_cursor = (end < builds_source_count).then(|| end.to_string());
    let truncated = next_cursor.is_some();
    let output = json!({
        "job": {
            "name": detail.name,
            "full_name": detail.full_name,
            "description_present": !detail.description.is_empty(),
            "description_summary": audit_json_summary(&json!(detail.description)),
            "url_omitted": !detail.url.is_empty(),
            "buildable": detail.buildable,
            "in_queue": detail.in_queue,
        },
        "builds": page_builds,
        "item_count": page_builds.len(),
        "source_item_count": builds_source_count,
        "limit": page.limit,
        "cursor": page.cursor,
        "next_cursor": next_cursor,
        "truncated": truncated,
    });
    let summary = json!({
        "job": output["job"],
        "item_count": output["item_count"],
        "source_item_count": output["source_item_count"],
        "truncated": truncated,
        "next_cursor": output["next_cursor"],
    });
    let output_page = truncated.then(|| InvocationOutputPage {
        next_cursor: output["next_cursor"].as_str().map(str::to_string),
    });
    result(invocation_id, output, summary, output_page)
}

fn activity_result(
    invocation_id: String,
    activity: Activity,
    page: PageRequest,
) -> CapabilityInvocationResult {
    let running_source_count = activity.running.len();
    let queue_source_count = activity.queue.len();
    let running_end = page
        .offset
        .saturating_add(page.limit)
        .min(running_source_count);
    let queue_end = page
        .offset
        .saturating_add(page.limit)
        .min(queue_source_count);
    let running = if page.offset >= running_source_count {
        Vec::new()
    } else {
        activity.running[page.offset..running_end]
            .iter()
            .map(running_json)
            .collect::<Vec<_>>()
    };
    let queue = if page.offset >= queue_source_count {
        Vec::new()
    } else {
        activity.queue[page.offset..queue_end]
            .iter()
            .map(queue_json)
            .collect::<Vec<_>>()
    };
    let next_offset = running_end.max(queue_end);
    let truncated = running_end < running_source_count || queue_end < queue_source_count;
    let next_cursor = truncated.then(|| next_offset.to_string());
    let output = json!({
        "running": running,
        "queue": queue,
        "running_count": running.len(),
        "queue_count": queue.len(),
        "running_source_count": running_source_count,
        "queue_source_count": queue_source_count,
        "limit": page.limit,
        "cursor": page.cursor,
        "next_cursor": next_cursor,
        "truncated": truncated,
    });
    let summary = json!({
        "running_count": output["running_count"],
        "queue_count": output["queue_count"],
        "running_source_count": running_source_count,
        "queue_source_count": queue_source_count,
        "truncated": truncated,
        "next_cursor": output["next_cursor"],
    });
    let output_page = truncated.then(|| InvocationOutputPage {
        next_cursor: output["next_cursor"].as_str().map(str::to_string),
    });
    result(invocation_id, output, summary, output_page)
}

fn pipeline_result(
    invocation_id: String,
    run: PipelineRun,
    page: PageRequest,
) -> CapabilityInvocationResult {
    let stages = run.stages.iter().map(stage_json).collect::<Vec<_>>();
    let metadata = json!({
        "job_full_name": run.job_full_name,
        "build_number": run.build_number,
        "status": run.status,
        "start_time_millis": run.start_time_millis,
        "duration_millis": run.duration_millis,
    });
    paged_result(invocation_id, "stages", stages, page, metadata)
}

fn dry_run_result(
    invocation_id: String,
    operation: &str,
    details: Value,
) -> CapabilityInvocationResult {
    let output = json!({
        "ok": true,
        "operation": operation,
        "dry_run": true,
        "would_execute": true,
        "destructive": true,
        "details": details,
    });
    result(
        invocation_id,
        output,
        json!({ "operation": operation, "dry_run": true }),
        None,
    )
}

fn mutation_result(
    invocation_id: String,
    operation: &str,
    details: Value,
) -> CapabilityInvocationResult {
    let output = json!({
        "ok": true,
        "operation": operation,
        "dry_run": false,
        "destructive": true,
        "details": details,
    });
    result(
        invocation_id,
        output,
        json!({ "operation": operation, "dry_run": false }),
        None,
    )
}

fn result(
    invocation_id: String,
    output: Value,
    output_summary: Value,
    page: Option<InvocationOutputPage>,
) -> CapabilityInvocationResult {
    CapabilityInvocationResult {
        invocation_id,
        status: InvocationStatus::Succeeded,
        output,
        output_summary,
        page,
    }
}

fn job_json(job: &JobSummary) -> Value {
    json!({
        "name": job.name,
        "status": job.status_label(),
        "running": job.is_running(),
        "folder": job.is_folder(),
        "class": job.class,
        "url_omitted": !job.url.is_empty(),
    })
}

fn build_json(build: &BuildSummary) -> Value {
    json!({
        "number": build.number,
        "result": build.result,
        "result_label": build.result_label(),
        "timestamp": build.timestamp,
        "duration": build.duration,
        "building": build.building,
        "url_omitted": !build.url.is_empty(),
    })
}

fn running_json(build: &RunningBuild) -> Value {
    json!({
        "job_name": build.job_name,
        "job_full_name": build.job_full_name,
        "build_number": build.build_number,
        "node": build.node,
        "executor": build.executor,
        "timestamp": build.timestamp,
        "elapsed_millis": build.elapsed_millis,
        "progress": build.progress,
        "url_omitted": !build.url.is_empty(),
    })
}

fn queue_json(item: &QueueItem) -> Value {
    json!({
        "id": item.id,
        "job_name": item.job_name,
        "job_full_name": item.job_full_name,
        "why": item.why,
        "why_summary": audit_json_summary(&json!(item.why)),
        "in_queue_since": item.in_queue_since,
        "blocked": item.blocked,
        "buildable": item.buildable,
        "stuck": item.stuck,
    })
}

fn stage_json(stage: &PipelineStage) -> Value {
    json!({
        "id": stage.id,
        "name": stage.name,
        "status": stage.status,
        "start_time_millis": stage.start_time_millis,
        "duration_millis": stage.duration_millis,
        "running": stage.is_running(),
        "parallel_stage_count": stage.parallel.len(),
    })
}

fn empty_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false
    })
}

fn list_schema(item_key: &str, item_schema: Value) -> Value {
    json!({
        "type": "object",
        "required": [
            item_key,
            "item_count",
            "source_item_count",
            "limit",
            "cursor",
            "next_cursor",
            "truncated",
            "metadata"
        ],
        "properties": {
            item_key: { "type": "array", "items": item_schema },
            "item_count": { "type": "integer", "minimum": 0 },
            "source_item_count": { "type": "integer", "minimum": 0 },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_LIMIT },
            "cursor": { "type": ["string", "null"] },
            "next_cursor": { "type": ["string", "null"] },
            "truncated": { "type": "boolean" },
            "metadata": { "type": ["object", "null"] }
        },
        "additionalProperties": false
    })
}

fn job_schema() -> Value {
    json!({
        "type": "object",
        "required": ["name", "status", "running", "folder", "class", "url_omitted"],
        "properties": {
            "name": { "type": "string" },
            "status": { "type": "string" },
            "running": { "type": "boolean" },
            "folder": { "type": "boolean" },
            "class": { "type": ["string", "null"] },
            "url_omitted": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn build_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "number",
            "result",
            "result_label",
            "timestamp",
            "duration",
            "building",
            "url_omitted"
        ],
        "properties": {
            "number": { "type": "integer", "minimum": 0 },
            "result": { "type": ["string", "null"] },
            "result_label": { "type": "string" },
            "timestamp": { "type": "integer", "minimum": 0 },
            "duration": { "type": "integer", "minimum": 0 },
            "building": { "type": "boolean" },
            "url_omitted": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn job_detail_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "job",
            "builds",
            "item_count",
            "source_item_count",
            "limit",
            "cursor",
            "next_cursor",
            "truncated"
        ],
        "properties": {
            "job": { "type": "object" },
            "builds": { "type": "array", "items": build_schema() },
            "item_count": { "type": "integer", "minimum": 0 },
            "source_item_count": { "type": "integer", "minimum": 0 },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_LIMIT },
            "cursor": { "type": ["string", "null"] },
            "next_cursor": { "type": ["string", "null"] },
            "truncated": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn activity_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "running",
            "queue",
            "running_count",
            "queue_count",
            "running_source_count",
            "queue_source_count",
            "limit",
            "cursor",
            "next_cursor",
            "truncated"
        ],
        "properties": {
            "running": { "type": "array", "items": { "type": "object" } },
            "queue": { "type": "array", "items": { "type": "object" } },
            "running_count": { "type": "integer", "minimum": 0 },
            "queue_count": { "type": "integer", "minimum": 0 },
            "running_source_count": { "type": "integer", "minimum": 0 },
            "queue_source_count": { "type": "integer", "minimum": 0 },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_LIMIT },
            "cursor": { "type": ["string", "null"] },
            "next_cursor": { "type": ["string", "null"] },
            "truncated": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn console_limit_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_CONSOLE_LIMIT_BYTES,
        "default": DEFAULT_CONSOLE_LIMIT_BYTES
    })
}

fn console_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "job_full_name",
            "build_number",
            "start",
            "text",
            "bytes_returned",
            "source_bytes",
            "byte_limit",
            "truncated",
            "next_offset",
            "has_more"
        ],
        "properties": {
            "job_full_name": { "type": "string" },
            "build_number": { "type": "integer", "minimum": 0 },
            "start": { "type": "integer", "minimum": 0 },
            "text": { "type": "string" },
            "bytes_returned": { "type": "integer", "minimum": 0 },
            "source_bytes": { "type": "integer", "minimum": 0 },
            "byte_limit": { "type": "integer", "minimum": 1, "maximum": MAX_CONSOLE_LIMIT_BYTES },
            "truncated": { "type": "boolean" },
            "next_offset": { "type": "integer", "minimum": 0 },
            "has_more": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn pipeline_stage_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "id",
            "name",
            "status",
            "start_time_millis",
            "duration_millis",
            "running",
            "parallel_stage_count"
        ],
        "properties": {
            "id": { "type": "string" },
            "name": { "type": "string" },
            "status": { "type": "string" },
            "start_time_millis": { "type": "integer", "minimum": 0 },
            "duration_millis": { "type": "integer", "minimum": 0 },
            "running": { "type": "boolean" },
            "parallel_stage_count": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": false
    })
}

fn mutation_output_schema() -> Value {
    json!({
        "type": "object",
        "required": ["ok", "operation", "dry_run", "destructive", "details"],
        "properties": {
            "ok": { "type": "boolean" },
            "operation": { "type": "string" },
            "dry_run": { "type": "boolean" },
            "would_execute": { "type": "boolean" },
            "destructive": { "type": "boolean" },
            "details": { "type": "object" }
        },
        "additionalProperties": false
    })
}

fn required_string(input: &Value, field: &str) -> Result<String, CapabilityError> {
    optional_string(input, field)?.ok_or_else(|| {
        validation_error(
            "validation.input_field_required",
            "Required string input field is missing.",
            json!({ "field": field }),
        )
    })
}

fn optional_string(input: &Value, field: &str) -> Result<Option<String>, CapabilityError> {
    match input.get(field) {
        Some(value) if !value.is_string() => Err(validation_error(
            "validation.input_field_invalid",
            "Input field must be a string.",
            json!({ "field": field }),
        )),
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| {
                validation_error(
                    "validation.input_field_required",
                    "String input field cannot be empty.",
                    json!({ "field": field }),
                )
            }),
        None => Ok(None),
    }
}

fn required_u64(input: &Value, field: &str) -> Result<u64, CapabilityError> {
    optional_u64(input, field)?.ok_or_else(|| {
        validation_error(
            "validation.input_field_required",
            "Required integer input field is missing.",
            json!({ "field": field }),
        )
    })
}

fn optional_u64(input: &Value, field: &str) -> Result<Option<u64>, CapabilityError> {
    match input.get(field) {
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            validation_error(
                "validation.input_field_invalid",
                "Input field must be an unsigned integer.",
                json!({ "field": field }),
            )
        }),
        None => Ok(None),
    }
}

fn required_i64(input: &Value, field: &str) -> Result<i64, CapabilityError> {
    input.get(field).and_then(Value::as_i64).ok_or_else(|| {
        validation_error(
            "validation.input_field_invalid",
            "Input field must be an integer.",
            json!({ "field": field }),
        )
    })
}

fn requested_usize(
    input: &Value,
    field: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, CapabilityError> {
    let Some(value) = input.get(field) else {
        return Ok(default);
    };
    let Some(raw) = value.as_u64() else {
        return Err(validation_error(
            "validation.input_field_invalid",
            "Input field must be an integer.",
            json!({ "field": field }),
        ));
    };
    if raw < minimum as u64 || raw > maximum as u64 {
        return Err(validation_error(
            "validation.input_field_out_of_range",
            "Input field is outside the supported range.",
            json!({ "field": field, "minimum": minimum, "maximum": maximum }),
        ));
    }
    Ok(raw as usize)
}

#[derive(Debug)]
struct PageRequest {
    limit: usize,
    offset: usize,
    cursor: Option<String>,
}

fn page_request(page: Option<&Pagination>) -> Result<PageRequest, CapabilityError> {
    let Some(page) = page else {
        return Ok(PageRequest {
            limit: DEFAULT_PAGE_LIMIT,
            offset: 0,
            cursor: None,
        });
    };
    let limit = (page.limit as usize).clamp(1, MAX_PAGE_LIMIT);
    let offset = match &page.cursor {
        Some(cursor) => cursor.parse::<usize>().map_err(|_| {
            validation_error(
                "validation.cursor_invalid",
                "Jenkins cursor must be a numeric offset.",
                json!({ "cursor": cursor }),
            )
        })?,
        None => 0,
    };
    Ok(PageRequest {
        limit,
        offset,
        cursor: page.cursor.clone(),
    })
}

fn bounded_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn url_scheme(url: &str) -> Option<String> {
    url.split_once("://").map(|(scheme, _)| scheme.to_string())
}

fn auth_type(config: &JenkinsConfig) -> &'static str {
    match config.auth {
        JenkinsAuth::None => "none",
        JenkinsAuth::Basic { .. } => "basic",
    }
}

fn validation_error(code: &str, message: &str, details: Value) -> CapabilityError {
    capability_error(
        CapabilityErrorCategory::Validation,
        code,
        message,
        details,
        None,
        false,
    )
}

fn unavailable_error(code: &str, message: &str, details: Value) -> CapabilityError {
    capability_error(
        CapabilityErrorCategory::Unavailable,
        code,
        message,
        details,
        None,
        true,
    )
}

fn target_error(config: &JenkinsConfig, code: &str, message: String) -> CapabilityError {
    let (message, redaction) = redact_jenkins_target_message(message, config);
    CapabilityError {
        category: CapabilityErrorCategory::TargetSystem,
        code: code.to_string(),
        message: "Jenkins target operation failed.".to_string(),
        details: json!({ "message": message.clone() }),
        target: Some(TargetSystemFailure {
            system: Some("jenkins".into()),
            code: Some(code.into()),
            message: Some(message),
        }),
        retryable: false,
        redaction,
    }
}

fn redact_jenkins_target_message(
    message: String,
    config: &JenkinsConfig,
) -> (String, RedactionStatus) {
    let original = message.clone();
    let mut redacted = message;

    redact_value(&mut redacted, &config.url);
    redact_value(&mut redacted, config.base_url());
    if let Some(authority) = url_authority(&config.url) {
        redact_value(&mut redacted, &authority);
    }
    if let Some(path) = url_path(&config.url) {
        redact_value(&mut redacted, &path);
    }

    match &config.auth {
        JenkinsAuth::Basic { username, token } => {
            redact_value(&mut redacted, username);
            redact_value(&mut redacted, token);
        }
        JenkinsAuth::None => {}
    }

    let redaction = if redacted != original {
        RedactionStatus::Applied
    } else {
        RedactionStatus::NotRequired
    };
    (redacted, redaction)
}

fn url_authority(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest
        .split(['/', '?'])
        .next()
        .filter(|authority| !authority.is_empty())?;
    Some(authority.to_string())
}

fn url_path(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let (_, path_and_query) = rest.split_once('/')?;
    path_and_query
        .split('?')
        .next()
        .filter(|path| path.len() >= 4)
        .map(str::to_string)
}

fn redact_value(message: &mut String, sensitive: &str) {
    if sensitive.len() >= 4 && message.contains(sensitive) {
        *message = message.replace(sensitive, "<redacted>");
    }
}

fn capability_error(
    category: CapabilityErrorCategory,
    code: &str,
    message: &str,
    details: Value,
    target: Option<TargetSystemFailure>,
    retryable: bool,
) -> CapabilityError {
    CapabilityError {
        category,
        code: code.to_string(),
        message: message.to_string(),
        details,
        target,
        retryable,
        redaction: RedactionStatus::NotRequired,
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;
    use voidb_core::{
        ActorRef, ActorType, ConnectionInstancePurpose, ConnectionProfileRef, InstanceReusePolicy,
        InvocationConnectionTarget, InvocationControls,
    };

    use super::*;

    #[test]
    fn catalog_marks_side_effects_as_destructive_dry_run() {
        let capabilities = jenkins_capabilities();
        for id in ["trigger_build", "abort_build", "cancel_queue_item"] {
            let capability = capabilities
                .iter()
                .find(|capability| capability.id == id)
                .expect("capability");
            assert!(capability.destructive);
            assert!(capability.supports_dry_run);
            assert_eq!(
                capability.effective_risk(),
                CapabilityRiskLevel::Destructive
            );
        }

        let console = capabilities
            .iter()
            .find(|capability| capability.id == "console")
            .expect("console capability");
        assert!(!console.destructive);
        assert!(!console.supports_dry_run);
    }

    #[tokio::test]
    async fn trigger_dry_run_does_not_open_jenkins_connection() {
        let config = test_config();
        let mut invocation =
            invocation("trigger_build", json!({ "job_full_name": "folder/build" }));
        invocation.controls.dry_run = true;

        let result = invoke_jenkins_capability(&config, invocation)
            .await
            .expect("dry-run");
        let encoded = serde_json::to_string(&result).expect("serialize");

        assert_eq!(result.output["dry_run"], true);
        assert_eq!(result.output["operation"], "trigger_build");
        assert_eq!(result.output["details"]["job_full_name"], "folder/build");
        assert!(!encoded.contains("jenkins-token"));
        assert!(!encoded.contains("ci.example.invalid"));
    }

    #[tokio::test]
    async fn diagnostics_do_not_open_jenkins_connection_or_expose_secrets() {
        let config = test_config();

        let result = invoke_jenkins_capability(&config, invocation("diagnostics", json!({})))
            .await
            .expect("diagnostics");
        let encoded = serde_json::to_string(&result).expect("serialize");

        assert_eq!(result.output["url_configured"], true);
        assert_eq!(result.output["url_scheme"], "https");
        assert_eq!(result.output["auth_type"], "basic");
        assert_eq!(result.output["network_checked"], false);
        assert!(!encoded.contains("jenkins-token"));
        assert!(!encoded.contains("ci.example.invalid"));
    }

    #[test]
    fn console_text_bound_preserves_utf8() {
        let (text, truncated) = bounded_text("ab🔥cd", 5);

        assert!(truncated);
        assert_eq!(text, "ab");
    }

    #[test]
    fn target_error_redacts_profile_values() {
        let config = JenkinsConfig {
            url: "https://ci.example.invalid/jenkins/private".into(),
            auth: JenkinsAuth::Basic {
                username: "fixture-agent".into(),
                token: "jenkins-secret-token".into(),
            },
            timeout: 1,
            verify_ssl: false,
        };

        let error = target_error(
            &config,
            "jenkins.test_failed",
            "failed https://ci.example.invalid/jenkins/private/api/json for fixture-agent with jenkins-secret-token".into(),
        );
        let encoded = serde_json::to_string(&error).expect("serialize");

        assert_eq!(error.redaction, RedactionStatus::Applied);
        assert!(encoded.contains("<redacted>"));
        for sample in [
            "ci.example.invalid",
            "jenkins/private",
            "fixture-agent",
            "jenkins-secret-token",
        ] {
            assert!(
                !encoded.contains(sample),
                "target error exposed Jenkins profile value {sample}: {encoded}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_wrong_plugin_id() {
        let config = test_config();
        let mut invocation = invocation("diagnostics", json!({}));
        invocation.plugin_id = "docker".into();

        let error = invoke_jenkins_capability(&config, invocation)
            .await
            .expect_err("plugin mismatch");

        assert_eq!(error.category, CapabilityErrorCategory::Validation);
        assert_eq!(error.code, "validation.plugin_mismatch");
    }

    fn test_config() -> JenkinsConfig {
        JenkinsConfig {
            url: "https://ci.example.invalid/jenkins".into(),
            auth: JenkinsAuth::Basic {
                username: "agent".into(),
                token: "jenkins-token".into(),
            },
            timeout: 1,
            verify_ssl: false,
        }
    }

    fn invocation(capability_id: &str, input: Value) -> CapabilityInvocation {
        CapabilityInvocation {
            id: format!("invoke-{}", capability_id),
            plugin_id: PLUGIN_ID.into(),
            capability_id: capability_id.into(),
            connection: InvocationConnectionTarget::FromProfile {
                profile: ConnectionProfileRef::Name("jenkins".into()),
                purpose: ConnectionInstancePurpose::CapabilityInvocation,
                reuse: InstanceReusePolicy::Allow,
                options: Value::Null,
            },
            input,
            controls: InvocationControls::default(),
            actor: Some(ActorRef {
                id: "test-agent".into(),
                actor_type: ActorType::Agent,
            }),
            requested_at: Utc::now(),
        }
    }
}
