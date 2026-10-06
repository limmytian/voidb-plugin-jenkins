//! Bounded Jenkins live workflows exposed through the generic agent broker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use voidb_core::{
    AGENT_LIVE_SESSION_PROTOCOL_VERSION, AgentLiveSessionAuditIdentity,
    AgentLiveSessionBufferOverflow, AgentLiveSessionBufferPolicy, AgentLiveSessionCallCancellation,
    AgentLiveSessionCancelBehavior, AgentLiveSessionCloseEffect, AgentLiveSessionContract,
    AgentLiveSessionControlPolicy, AgentLiveSessionCursor, AgentLiveSessionCursorKind,
    AgentLiveSessionEventBuffer, AgentLiveSessionEventKind, AgentLiveSessionKind,
    AgentLiveSessionOperations, AgentLiveSessionReadRequest, AgentLiveSessionReconnectMode,
    AgentLiveSessionReconnectPolicy, AgentLiveSessionResourceDescriptor,
    AgentLiveSessionResumeMode, AgentLiveSessionStartRequest, AgentSessionCallRequest,
    AgentSessionCallResult, AgentSessionOpenContext, CapabilityRiskLevel, PluginAgentSession,
    PluginAgentSessionFactory, PluginSessionError, PluginSessionErrorCode, PluginSessionHealth,
    PluginSessionPurpose, RedactionStatus, RedactionTarget, collect_redaction_targets,
    redact_text_with_targets,
};

use crate::config::JenkinsConfig;
use crate::service::JenkinsService;

pub(crate) const CONSOLE_FOLLOW_CAPABILITY: &str = "jenkins.console_follow";
pub(crate) const BUILD_WAIT_CAPABILITY: &str = "jenkins.build_wait";
pub(crate) const QUEUE_WATCH_CAPABILITY: &str = "jenkins.queue_watch";

const MIN_POLL_INTERVAL_MS: u64 = 250;
const DEFAULT_POLL_INTERVAL_MS: u64 = 1_000;
const MAX_POLL_INTERVAL_MS: u64 = 10_000;
const MAX_CONSOLE_EVENT_TEXT_BYTES: usize = 48 * 1024;

pub struct JenkinsAgentSessionFactory {
    config: JenkinsConfig,
    redaction_targets: Arc<Vec<RedactionTarget>>,
}

impl JenkinsAgentSessionFactory {
    pub fn new(config: JenkinsConfig) -> Self {
        let redaction_targets = serde_json::to_value(&config)
            .map(|value| collect_redaction_targets(&value))
            .unwrap_or_default();
        Self {
            config,
            redaction_targets: Arc::new(redaction_targets),
        }
    }
}

#[async_trait]
impl PluginAgentSessionFactory for JenkinsAgentSessionFactory {
    fn plugin_id(&self) -> &str {
        "jenkins"
    }

    async fn open(
        &self,
        context: AgentSessionOpenContext,
    ) -> Result<Arc<dyn PluginAgentSession>, PluginSessionError> {
        let capability = single_live_capability(&context)?;
        let (purpose, contract) = jenkins_live_session_contract(capability).ok_or_else(|| {
            session_error(
                PluginSessionErrorCode::PolicyDenied,
                "The Jenkins session binding is not a live-session family.",
            )
        })?;
        if context.binding.purpose != purpose {
            return Err(session_error(
                PluginSessionErrorCode::BindingMismatch,
                "The Jenkins live-session purpose does not match its capability family.",
            ));
        }
        contract.validate(&context.binding.allowed_capabilities)?;
        contract.validate_start(&context.request.input)?;
        let start: AgentLiveSessionStartRequest =
            serde_json::from_value(context.request.input.clone()).map_err(|_| {
                session_error(
                    PluginSessionErrorCode::PolicyDenied,
                    "The Jenkins live-session start envelope is invalid.",
                )
            })?;
        let policy = start
            .buffer
            .clone()
            .unwrap_or_else(|| contract.buffer.clone());
        let buffer = AgentLiveSessionEventBuffer::new(contract.clone(), policy)?;
        let service = JenkinsService::new_direct(&self.config).map_err(|_| {
            session_error(
                PluginSessionErrorCode::OwnerUnavailable,
                "The Jenkins client could not be created.",
            )
        })?;
        service.ping().await.map_err(|_| {
            session_error(
                PluginSessionErrorCode::OwnerUnavailable,
                "The Jenkins server could not be reached or authenticated.",
            )
        })?;

        let producer_buffer = buffer.clone();
        let targets = Arc::clone(&self.redaction_targets);
        let capability_owned = capability.to_string();
        let task = tokio::spawn(async move {
            match capability_owned.as_str() {
                CONSOLE_FOLLOW_CAPABILITY => {
                    run_console_follow(service, start, producer_buffer.clone(), targets).await
                }
                BUILD_WAIT_CAPABILITY => {
                    run_build_wait(service, start, producer_buffer.clone()).await
                }
                QUEUE_WATCH_CAPABILITY => {
                    run_queue_watch(service, start, producer_buffer.clone(), targets).await
                }
                _ => {}
            }
            producer_buffer.close_source().await;
        });

        Ok(Arc::new(JenkinsAgentLiveSession {
            capability: capability.to_string(),
            buffer,
            cancellations: AgentLiveSessionCallCancellation::default(),
            task: Mutex::new(Some(task)),
            closed: AtomicBool::new(false),
        }))
    }
}

struct JenkinsAgentLiveSession {
    capability: String,
    buffer: AgentLiveSessionEventBuffer,
    cancellations: AgentLiveSessionCallCancellation,
    task: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

#[async_trait]
impl PluginAgentSession for JenkinsAgentLiveSession {
    async fn call(
        &self,
        request: AgentSessionCallRequest,
    ) -> Result<AgentSessionCallResult, PluginSessionError> {
        if request.capability != self.capability {
            return Err(session_error(
                PluginSessionErrorCode::PolicyDenied,
                "The Jenkins call is outside this live-session binding.",
            ));
        }
        let mut read = if request.input.is_null() {
            AgentLiveSessionReadRequest::default()
        } else {
            serde_json::from_value(request.input.clone()).map_err(|_| {
                session_error(
                    PluginSessionErrorCode::PolicyDenied,
                    "The Jenkins live-session read request is invalid.",
                )
            })?
        };
        read.max_bytes = read.max_bytes.min(request.output_limit_bytes);
        let call_id = request.call_id.clone();
        let batch = self
            .cancellations
            .run(&call_id, self.buffer.read(&read))
            .await?;
        let output = serde_json::to_value(batch).map_err(|_| {
            session_error(
                PluginSessionErrorCode::RedactionFailed,
                "The Jenkins live-session batch could not be serialized.",
            )
        })?;
        AgentSessionCallResult::bounded(call_id, output, request.output_limit_bytes)
    }

    async fn health(&self) -> Result<PluginSessionHealth, PluginSessionError> {
        Ok(if self.closed.load(Ordering::Acquire) {
            PluginSessionHealth::Closed
        } else {
            PluginSessionHealth::Ready
        })
    }

    async fn cancel(&self, call_id: &str) -> Result<(), PluginSessionError> {
        self.cancellations.cancel(call_id).await;
        Ok(())
    }

    async fn close(&self, _reason: String) -> Result<(), PluginSessionError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.cancellations.close().await;
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.buffer.close_source().await;
        Ok(())
    }
}

pub(crate) fn jenkins_live_session_contract(
    capability: &str,
) -> Option<(PluginSessionPurpose, AgentLiveSessionContract)> {
    let (purpose, kind, resource_type, identity_schema, identity_fields, buffer, reconnect) =
        match capability {
            CONSOLE_FOLLOW_CAPABILITY => (
                PluginSessionPurpose::LogStream,
                AgentLiveSessionKind::ProgressiveOutput,
                "jenkins_build",
                job_build_identity_schema(),
                vec!["/job_full_name".into(), "/build_number".into()],
                AgentLiveSessionBufferPolicy {
                    max_events: 5_000,
                    max_bytes: 5 * 1024 * 1024,
                    overflow: AgentLiveSessionBufferOverflow::DropOldest,
                },
                AgentLiveSessionReconnectPolicy {
                    mode: AgentLiveSessionReconnectMode::Transient,
                    max_attempts: 8,
                    initial_backoff_ms: 250,
                    max_backoff_ms: 10_000,
                    resume: AgentLiveSessionResumeMode::ExactCursor,
                    cursor_kind: Some(AgentLiveSessionCursorKind::ByteOffset),
                },
            ),
            BUILD_WAIT_CAPABILITY => (
                PluginSessionPurpose::WatchStream,
                AgentLiveSessionKind::Wait,
                "jenkins_build",
                job_build_identity_schema(),
                vec!["/job_full_name".into(), "/build_number".into()],
                state_buffer(),
                restart_reconnect_policy(),
            ),
            QUEUE_WATCH_CAPABILITY => (
                PluginSessionPurpose::WatchStream,
                AgentLiveSessionKind::Queue,
                "jenkins_queue_item",
                json!({
                    "type": "object",
                    "required": ["queue_id"],
                    "properties": { "queue_id": { "type": "integer", "minimum": 0 } },
                    "additionalProperties": false
                }),
                vec!["/queue_id".into()],
                state_buffer(),
                restart_reconnect_policy(),
            ),
            _ => return None,
        };
    Some((
        purpose,
        AgentLiveSessionContract {
            protocol_version: AGENT_LIVE_SESSION_PROTOCOL_VERSION,
            kind,
            resource: AgentLiveSessionResourceDescriptor {
                resource_type: resource_type.into(),
                identity_schema,
                identity_fields,
                audit_identity: AgentLiveSessionAuditIdentity::Fingerprint,
            },
            start_parameters_schema: json!({
                "type": "object",
                "properties": {
                    "poll_interval_ms": {
                        "type": "integer",
                        "minimum": MIN_POLL_INTERVAL_MS,
                        "maximum": MAX_POLL_INTERVAL_MS,
                        "default": DEFAULT_POLL_INTERVAL_MS
                    }
                },
                "additionalProperties": false
            }),
            event_schema: json!({
                "type": "object",
                "maxProperties": 16
            }),
            operations: AgentLiveSessionOperations {
                events: capability.into(),
                input: None,
                resize: None,
                signal: None,
            },
            buffer,
            reconnect,
            delivery: Default::default(),
            control: AgentLiveSessionControlPolicy {
                cancel: AgentLiveSessionCancelBehavior::CallOnly,
                close: AgentLiveSessionCloseEffect::StopObservation,
            },
            start_risk: CapabilityRiskLevel::ReadOnly,
        },
    ))
}

fn job_build_identity_schema() -> Value {
    json!({
        "type": "object",
        "required": ["job_full_name", "build_number"],
        "properties": {
            "job_full_name": { "type": "string", "minLength": 1, "maxLength": 512 },
            "build_number": { "type": "integer", "minimum": 0 }
        },
        "additionalProperties": false
    })
}

fn state_buffer() -> AgentLiveSessionBufferPolicy {
    AgentLiveSessionBufferPolicy {
        max_events: 32,
        max_bytes: 256 * 1024,
        overflow: AgentLiveSessionBufferOverflow::Coalesce,
    }
}

fn restart_reconnect_policy() -> AgentLiveSessionReconnectPolicy {
    AgentLiveSessionReconnectPolicy {
        mode: AgentLiveSessionReconnectMode::Transient,
        max_attempts: 8,
        initial_backoff_ms: 250,
        max_backoff_ms: 10_000,
        resume: AgentLiveSessionResumeMode::Restart,
        cursor_kind: None,
    }
}

fn single_live_capability(context: &AgentSessionOpenContext) -> Result<&str, PluginSessionError> {
    if context.binding.allowed_capabilities.len() != 1 {
        return Err(session_error(
            PluginSessionErrorCode::PolicyDenied,
            "A Jenkins live session requires exactly one capability family.",
        ));
    }
    Ok(context.binding.allowed_capabilities[0].as_str())
}

async fn run_console_follow(
    service: JenkinsService,
    start: AgentLiveSessionStartRequest,
    buffer: AgentLiveSessionEventBuffer,
    redaction_targets: Arc<Vec<RedactionTarget>>,
) {
    let Ok(job) = required_string(&start.resource, "job_full_name") else {
        push_terminal_error(&buffer, "invalid_resource").await;
        return;
    };
    let Ok(build) = required_u64(&start.resource, "build_number") else {
        push_terminal_error(&buffer, "invalid_resource").await;
        return;
    };
    let poll = poll_interval(&start.parameters).unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    let mut offset = start
        .resume_from
        .as_ref()
        .and_then(|cursor| cursor.value.parse::<u64>().ok())
        .unwrap_or(0);

    loop {
        match service.fetch_console(&job, build, offset).await {
            Ok(chunk) => {
                if chunk.next_offset < offset
                    || (!chunk.text.is_empty() && chunk.next_offset == offset)
                {
                    push_terminal_error(&buffer, "invalid_progressive_offset").await;
                    return;
                }
                if !chunk.text.is_empty() {
                    let mut emitted_source_bytes = 0u64;
                    for text in split_text(&chunk.text, MAX_CONSOLE_EVENT_TEXT_BYTES) {
                        let source_bytes = text.len() as u64;
                        emitted_source_bytes = emitted_source_bytes.saturating_add(source_bytes);
                        let event_offset = offset
                            .saturating_add(emitted_source_bytes)
                            .min(chunk.next_offset);
                        let (text, redaction) = redact_text_with_targets(&text, &redaction_targets);
                        if buffer
                            .push(
                                chrono::Utc::now(),
                                AgentLiveSessionEventKind::Data,
                                json!({
                                    "text": text,
                                    "source_bytes": source_bytes,
                                    "start_offset": event_offset.saturating_sub(source_bytes),
                                    "next_offset": event_offset,
                                    "has_more": chunk.has_more
                                }),
                                Some(AgentLiveSessionCursor {
                                    kind: AgentLiveSessionCursorKind::ByteOffset,
                                    value: event_offset.to_string(),
                                    scope: None,
                                }),
                                redaction,
                                false,
                            )
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                offset = chunk.next_offset;
                if !chunk.has_more {
                    let _ = buffer
                        .push(
                            chrono::Utc::now(),
                            AgentLiveSessionEventKind::End,
                            json!({ "state": "completed", "next_offset": offset }),
                            Some(AgentLiveSessionCursor {
                                kind: AgentLiveSessionCursorKind::ByteOffset,
                                value: offset.to_string(),
                                scope: None,
                            }),
                            RedactionStatus::NotRequired,
                            true,
                        )
                        .await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(poll)).await;
            }
            Err(error) => {
                if !retry_after_error(&buffer, &error).await {
                    return;
                }
            }
        }
    }
}

async fn run_build_wait(
    service: JenkinsService,
    start: AgentLiveSessionStartRequest,
    buffer: AgentLiveSessionEventBuffer,
) {
    let Ok(job) = required_string(&start.resource, "job_full_name") else {
        push_terminal_error(&buffer, "invalid_resource").await;
        return;
    };
    let Ok(build) = required_u64(&start.resource, "build_number") else {
        push_terminal_error(&buffer, "invalid_resource").await;
        return;
    };
    let poll = poll_interval(&start.parameters).unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    loop {
        match service.fetch_build_state(&job, build).await {
            Ok(state) => {
                let terminal = !state.building;
                let kind = if terminal {
                    AgentLiveSessionEventKind::End
                } else {
                    AgentLiveSessionEventKind::State
                };
                if buffer
                    .push(
                        chrono::Utc::now(),
                        kind,
                        json!({
                            "state": if terminal { "completed" } else { "building" },
                            "building": state.building,
                            "result": state.result,
                            "timestamp": state.timestamp,
                            "duration": state.duration
                        }),
                        None,
                        RedactionStatus::NotRequired,
                        terminal,
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                if terminal {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(poll)).await;
            }
            Err(error) => {
                if !retry_after_error(&buffer, &error).await {
                    return;
                }
            }
        }
    }
}

async fn run_queue_watch(
    service: JenkinsService,
    start: AgentLiveSessionStartRequest,
    buffer: AgentLiveSessionEventBuffer,
    redaction_targets: Arc<Vec<RedactionTarget>>,
) {
    let Ok(queue_id) = required_i64(&start.resource, "queue_id") else {
        push_terminal_error(&buffer, "invalid_resource").await;
        return;
    };
    let poll = poll_interval(&start.parameters).unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    loop {
        match service.fetch_queue_tracking_state(queue_id).await {
            Ok(state) => {
                let (state_name, terminal) = if state.cancelled {
                    ("cancelled", true)
                } else if state.executable_number.is_some() {
                    ("started", true)
                } else if state.missing {
                    ("missing", true)
                } else {
                    ("queued", false)
                };
                let (why, redaction) = redact_text_with_targets(&state.why, &redaction_targets);
                let kind = if terminal {
                    AgentLiveSessionEventKind::End
                } else {
                    AgentLiveSessionEventKind::State
                };
                if buffer
                    .push(
                        chrono::Utc::now(),
                        kind,
                        json!({
                            "state": state_name,
                            "executable_number": state.executable_number,
                            "why": why,
                            "blocked": state.blocked,
                            "buildable": state.buildable,
                            "stuck": state.stuck
                        }),
                        None,
                        redaction,
                        terminal,
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                if terminal {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(poll)).await;
            }
            Err(error) => {
                if !retry_after_error(&buffer, &error).await {
                    return;
                }
            }
        }
    }
}

async fn retry_after_error(buffer: &AgentLiveSessionEventBuffer, error: &anyhow::Error) -> bool {
    let class = target_error_class(error);
    if matches!(class, "authentication" | "authorization" | "not_found") {
        push_terminal_error(buffer, class).await;
        return false;
    }
    let attempt = match buffer.record_reconnect().await {
        Ok(attempt) => attempt,
        Err(_) => {
            push_terminal_error(buffer, "retry_exhausted").await;
            return false;
        }
    };
    if buffer
        .push(
            chrono::Utc::now(),
            AgentLiveSessionEventKind::Warning,
            json!({ "state": "reconnecting", "error_class": class, "attempt": attempt }),
            None,
            RedactionStatus::NotRequired,
            false,
        )
        .await
        .is_err()
    {
        return false;
    }
    let backoff = 250u64
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(5))
        .min(10_000);
    tokio::time::sleep(Duration::from_millis(backoff)).await;
    true
}

async fn push_terminal_error(buffer: &AgentLiveSessionEventBuffer, class: &str) {
    let _ = buffer
        .push(
            chrono::Utc::now(),
            AgentLiveSessionEventKind::Error,
            json!({ "state": "failed", "error_class": class }),
            None,
            RedactionStatus::NotRequired,
            true,
        )
        .await;
}

fn target_error_class(error: &anyhow::Error) -> &'static str {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("401") || message.contains("unauthorized") {
        "authentication"
    } else if message.contains("403") || message.contains("forbidden") {
        "authorization"
    } else if message.contains("404") || message.contains("not found") {
        "not_found"
    } else if message.contains("timeout") || message.contains("timed out") {
        "timeout"
    } else {
        "transport"
    }
}

fn required_string(value: &Value, key: &str) -> Result<String, PluginSessionError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            session_error(
                PluginSessionErrorCode::PolicyDenied,
                format!("Jenkins live-session resource field '{key}' is required."),
            )
        })
}

fn required_u64(value: &Value, key: &str) -> Result<u64, PluginSessionError> {
    value.get(key).and_then(Value::as_u64).ok_or_else(|| {
        session_error(
            PluginSessionErrorCode::PolicyDenied,
            format!("Jenkins live-session resource field '{key}' must be an unsigned integer."),
        )
    })
}

fn required_i64(value: &Value, key: &str) -> Result<i64, PluginSessionError> {
    value.get(key).and_then(Value::as_i64).ok_or_else(|| {
        session_error(
            PluginSessionErrorCode::PolicyDenied,
            format!("Jenkins live-session resource field '{key}' must be an integer."),
        )
    })
}

fn poll_interval(parameters: &Value) -> Result<u64, PluginSessionError> {
    let value = parameters
        .get("poll_interval_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    if !(MIN_POLL_INTERVAL_MS..=MAX_POLL_INTERVAL_MS).contains(&value) {
        return Err(session_error(
            PluginSessionErrorCode::PolicyDenied,
            format!(
                "poll_interval_ms must be between {MIN_POLL_INTERVAL_MS} and {MAX_POLL_INTERVAL_MS}."
            ),
        ));
    }
    Ok(value)
}

fn split_text(text: &str, max_bytes: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if !current.is_empty() && current.len().saturating_add(character.len_utf8()) > max_bytes {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn session_error(code: PluginSessionErrorCode, message: impl Into<String>) -> PluginSessionError {
    PluginSessionError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contracts_are_bounded_and_match_their_session_purpose() {
        for capability in [
            CONSOLE_FOLLOW_CAPABILITY,
            BUILD_WAIT_CAPABILITY,
            QUEUE_WATCH_CAPABILITY,
        ] {
            let (purpose, contract) =
                jenkins_live_session_contract(capability).expect("live contract");
            contract
                .validate(&[capability.to_string()])
                .expect("valid contract");
            assert!(matches!(
                purpose,
                PluginSessionPurpose::LogStream | PluginSessionPurpose::WatchStream
            ));
            assert!(contract.buffer.max_events <= 5_000);
            assert!(contract.buffer.max_bytes <= 5 * 1024 * 1024);
            assert_eq!(contract.start_risk, CapabilityRiskLevel::ReadOnly);
        }
    }

    #[test]
    fn console_chunks_preserve_utf8_boundaries_and_limits() {
        let chunks = split_text("a界b界c", 4);
        assert_eq!(chunks.concat(), "a界b界c");
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4));
    }

    #[test]
    fn target_errors_are_classified_without_returning_raw_bodies() {
        assert_eq!(
            target_error_class(&anyhow::anyhow!("HTTP 403: secret body")),
            "authorization"
        );
        assert_eq!(
            target_error_class(&anyhow::anyhow!("connection timed out")),
            "timeout"
        );
    }

    #[test]
    fn factory_redaction_targets_withhold_profile_secrets() {
        let secret = "jenkins-conformance-token";
        let factory = JenkinsAgentSessionFactory::new(JenkinsConfig {
            url: "https://jenkins.example.test".into(),
            auth: crate::config::JenkinsAuth::Basic {
                username: "fixture".into(),
                token: secret.into(),
            },
            timeout: 1,
            verify_ssl: true,
        });
        let (redacted, status) = redact_text_with_targets(
            &format!("target returned {secret}"),
            factory.redaction_targets.as_ref(),
        );
        assert_eq!(status, RedactionStatus::Applied);
        assert!(!redacted.contains(secret));
    }
}
