//! Jenkins fixture-backed capability smoke driver.
//!
//! This example is script-facing. It exercises the Jenkins plugin capability
//! surface against a disposable local Jenkins fixture using only the generated
//! job and build from the fixture environment.

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::time::{Duration, Instant, sleep};
use voidb_core::{
    ActorRef, ActorType, CapabilityError, CapabilityErrorCategory, CapabilityInvocation,
    CapabilityInvocationResult, InvocationAcknowledgement, InvocationConnectionTarget,
    InvocationControls, InvocationStatus, Pagination, RedactionStatus,
};
use voidb_plugin_jenkins::{JenkinsConfig, config::JenkinsAuth, invoke_jenkins_capability};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let config = config_from_env()?;
    let job = required_env("VOIDB_JENKINS_SMOKE_JOB")?;
    let build_number = required_env("VOIDB_JENKINS_SMOKE_BUILD")?
        .parse::<u64>()
        .context("VOIDB_JENKINS_SMOKE_BUILD must be numeric")?;
    ensure!(
        job.starts_with("voidb-fixture-"),
        "refusing Jenkins smoke outside a generated fixture job"
    );
    ensure!(
        build_number == 1,
        "refusing Jenkins smoke without generated build 1"
    );

    let diagnostics = invoke_checked(
        &config,
        "diagnostics",
        json!({}),
        false,
        false,
        None,
        "jenkins.diagnostics",
    )
    .await?;
    ensure_succeeded(&diagnostics, "jenkins.diagnostics")?;
    ensure!(
        diagnostics.output["url_configured"] == true
            && diagnostics.output["url_scheme"] == "http"
            && diagnostics.output["auth_type"] == "basic"
            && diagnostics.output["verify_ssl"] == false
            && diagnostics.output["network_checked"] == false,
        "diagnostics should return shape-only metadata: {}",
        diagnostics.output
    );
    ensure_result_excludes(&diagnostics, secret_samples(&config), "jenkins.diagnostics")?;

    for (capability, input) in [
        ("trigger_build", json!({ "job_full_name": &job })),
        (
            "abort_build",
            json!({ "job_full_name": &job, "build_number": build_number }),
        ),
        ("cancel_queue_item", json!({ "queue_id": 42 })),
    ] {
        let dry_run = invoke_checked(
            &unavailable_config(),
            capability,
            input,
            true,
            false,
            None,
            "jenkins destructive dry-run should not require a live target",
        )
        .await?;
        ensure_succeeded(&dry_run, capability)?;
        ensure!(
            dry_run.output["dry_run"] == true
                && dry_run.output["would_execute"] == true
                && dry_run.output["destructive"] == true,
            "{capability} dry-run output: {}",
            dry_run.output
        );
    }

    let jobs = invoke_checked(
        &config,
        "jobs",
        json!({}),
        false,
        false,
        Some(Pagination {
            limit: 20,
            cursor: None,
        }),
        "jenkins.jobs",
    )
    .await?;
    ensure_succeeded(&jobs, "jenkins.jobs")?;
    ensure_job_present(&jobs.output, &job)?;
    ensure_result_excludes(&jobs, secret_samples(&config), "jenkins.jobs")?;

    let detail = invoke_checked(
        &config,
        "job_detail",
        json!({ "job_full_name": &job }),
        false,
        false,
        Some(Pagination {
            limit: 10,
            cursor: None,
        }),
        "jenkins.job_detail",
    )
    .await?;
    ensure_succeeded(&detail, "jenkins.job_detail")?;
    ensure!(
        detail.output["job"]["full_name"].as_str() == Some(job.as_str())
            && detail.output["job"]["url_omitted"] == true
            && detail.output["job"]["buildable"] == true,
        "job detail should return bounded job metadata: {}",
        detail.output
    );
    ensure_build_result(&detail.output, build_number, "SUCCESS")?;
    ensure_result_excludes(&detail, secret_samples(&config), "jenkins.job_detail")?;

    let activity = invoke_checked(
        &config,
        "activity",
        json!({}),
        false,
        false,
        Some(Pagination {
            limit: 20,
            cursor: None,
        }),
        "jenkins.activity",
    )
    .await?;
    ensure_succeeded(&activity, "jenkins.activity")?;
    ensure!(
        activity.output["running_count"].as_u64().is_some()
            && activity.output["queue_count"].as_u64().is_some(),
        "activity should return bounded running and queue counts: {}",
        activity.output
    );

    let console = invoke_checked(
        &config,
        "console",
        json!({
            "job_full_name": &job,
            "build_number": build_number,
            "max_bytes": 4096
        }),
        false,
        false,
        None,
        "jenkins.console",
    )
    .await?;
    ensure_succeeded(&console, "jenkins.console")?;
    ensure!(
        console.output["build_number"].as_u64() == Some(build_number)
            && console.output["bytes_returned"]
                .as_u64()
                .unwrap_or_default()
                <= 4096
            && console.output["text"]
                .as_str()
                .is_some_and(|text| text.contains("fixture build complete")),
        "console should return bounded fixture output: {}",
        console.output_summary
    );
    ensure_result_excludes(
        &console,
        console_protected_samples(&config),
        "jenkins.console",
    )?;

    let dry_trigger = invoke_checked(
        &unavailable_config(),
        "trigger_build",
        json!({ "job_full_name": &job }),
        true,
        false,
        None,
        "jenkins.trigger_build dry-run",
    )
    .await?;
    ensure_succeeded(&dry_trigger, "jenkins.trigger_build dry-run")?;
    ensure!(
        dry_trigger.output["dry_run"] == true,
        "trigger dry-run output"
    );

    let trigger = invoke_checked(
        &config,
        "trigger_build",
        json!({ "job_full_name": &job }),
        false,
        true,
        None,
        "jenkins.trigger_build acknowledged",
    )
    .await?;
    ensure_succeeded(&trigger, "jenkins.trigger_build acknowledged")?;
    ensure!(
        trigger.output["dry_run"] == false
            && trigger.output["operation"] == "trigger_build"
            && trigger.output["details"]["job_full_name"].as_str() == Some(job.as_str()),
        "trigger output should be scoped to the generated job: {}",
        trigger.output
    );
    ensure_result_excludes(&trigger, secret_samples(&config), "jenkins.trigger_build")?;

    let triggered_build = build_number + 1;
    let triggered_detail = wait_for_build_result(&config, &job, triggered_build, "SUCCESS").await?;
    ensure_result_excludes(
        &triggered_detail,
        secret_samples(&config),
        "jenkins.triggered job_detail",
    )?;

    ensure_missing_job_error_redacts(&config, &job).await?;
    ensure_bad_auth_redacts(&config).await?;
    ensure_unavailable_target_redacts().await?;

    println!("jenkins fixture capability smoke passed");
    println!(
        "capabilities: diagnostics, jobs, job_detail, activity, console, trigger_build dry-run, trigger_build acknowledged, abort_build dry-run, cancel_queue_item dry-run"
    );
    println!("fixture_job: generated");
    Ok(())
}

fn config_from_env() -> Result<JenkinsConfig> {
    let verify_ssl = std::env::var("VOIDB_JENKINS_SMOKE_VERIFY_SSL")
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"));
    Ok(JenkinsConfig {
        url: required_env("VOIDB_JENKINS_SMOKE_URL")?,
        auth: JenkinsAuth::Basic {
            username: required_env("VOIDB_JENKINS_SMOKE_USER")?,
            token: required_env("VOIDB_JENKINS_SMOKE_TOKEN")?,
        },
        timeout: 10,
        verify_ssl,
    })
}

fn unavailable_config() -> JenkinsConfig {
    JenkinsConfig {
        url: "http://127.0.0.1:1/voidb-jenkins-secret-path".into(),
        auth: JenkinsAuth::Basic {
            username: "voidb_jenkins_unavailable_user".into(),
            token: "voidb-jenkins-unavailable-token".into(),
        },
        timeout: 1,
        verify_ssl: false,
    }
}

fn bad_auth_config(config: &JenkinsConfig) -> JenkinsConfig {
    let mut bad = config.clone();
    if let JenkinsAuth::Basic { token, .. } = &mut bad.auth {
        token.push_str("-wrong");
    }
    bad
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| format!("{name} is required"))
}

async fn invoke(
    config: &JenkinsConfig,
    capability_id: &str,
    input: Value,
    dry_run: bool,
    acknowledged: bool,
    page: Option<Pagination>,
) -> std::result::Result<CapabilityInvocationResult, CapabilityError> {
    invoke_jenkins_capability(
        config,
        CapabilityInvocation {
            id: format!("jenkins-fixture-smoke-{capability_id}"),
            plugin_id: "jenkins".into(),
            capability_id: capability_id.into(),
            connection: InvocationConnectionTarget::Stateless,
            input,
            controls: InvocationControls {
                dry_run,
                acknowledgement: acknowledged.then(acknowledgement),
                page,
                ..InvocationControls::default()
            },
            actor: Some(actor()),
            requested_at: Utc::now(),
        },
    )
    .await
}

async fn invoke_checked(
    config: &JenkinsConfig,
    capability_id: &str,
    input: Value,
    dry_run: bool,
    acknowledged: bool,
    page: Option<Pagination>,
    label: &str,
) -> Result<CapabilityInvocationResult> {
    invoke(config, capability_id, input, dry_run, acknowledged, page)
        .await
        .map_err(|error| {
            let error_json = serde_json::to_string(&error).unwrap_or_else(|_| format!("{error:?}"));
            anyhow::anyhow!("{label}: {error_json}")
        })
}

fn ensure_succeeded(result: &CapabilityInvocationResult, label: &str) -> Result<()> {
    ensure!(
        result.status == InvocationStatus::Succeeded,
        "{label} returned non-success status: {:?}",
        result.status
    );
    Ok(())
}

fn ensure_result_excludes(
    result: &CapabilityInvocationResult,
    samples: Vec<String>,
    label: &str,
) -> Result<()> {
    let text = serde_json::to_string(result)?;
    for sample in samples {
        ensure!(
            !text.contains(&sample),
            "{label} exposed protected Jenkins sample {sample}: {text}"
        );
    }
    Ok(())
}

fn ensure_error_excludes_config(
    error: &CapabilityError,
    config: &JenkinsConfig,
    label: &str,
) -> Result<()> {
    let text = serde_json::to_string(error)?;
    for sample in error_samples(config) {
        ensure!(
            !text.contains(&sample),
            "{label} error exposed Jenkins config material {sample}: {text}"
        );
    }
    ensure!(
        matches!(
            error.redaction,
            RedactionStatus::Applied | RedactionStatus::NotRequired
        ),
        "{label} redaction status should be non-failed: {:?}",
        error.redaction
    );
    Ok(())
}

fn secret_samples(config: &JenkinsConfig) -> Vec<String> {
    let mut samples = vec![config.url.clone()];
    if let Some(authority) = url_authority(&config.url) {
        samples.push(authority);
    }
    if let Some(path) = url_path(&config.url) {
        samples.push(path);
    }
    match &config.auth {
        JenkinsAuth::Basic { username, token } => {
            samples.push(username.clone());
            samples.push(token.clone());
        }
        JenkinsAuth::None => {}
    }
    samples
        .into_iter()
        .filter(|sample| sample.len() >= 4)
        .collect()
}

fn console_protected_samples(config: &JenkinsConfig) -> Vec<String> {
    let username = match &config.auth {
        JenkinsAuth::Basic { username, .. } => Some(username.as_str()),
        JenkinsAuth::None => None,
    };
    secret_samples(config)
        .into_iter()
        .filter(|sample| Some(sample.as_str()) != username)
        .collect()
}

fn error_samples(config: &JenkinsConfig) -> Vec<String> {
    secret_samples(config)
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

fn ensure_job_present(output: &Value, job: &str) -> Result<()> {
    let jobs = output["jobs"]
        .as_array()
        .context("jenkins.jobs output should include jobs array")?;
    ensure!(
        jobs.iter().any(|item| item["name"] == job),
        "jenkins.jobs did not include expected job {job}: {output}"
    );
    Ok(())
}

fn ensure_build_result(output: &Value, build_number: u64, result: &str) -> Result<()> {
    let builds = output["builds"]
        .as_array()
        .context("jenkins.job_detail output should include builds array")?;
    ensure!(
        builds.iter().any(|item| {
            item["number"].as_u64() == Some(build_number)
                && item["result"].as_str() == Some(result)
                && item["building"] == false
        }),
        "jenkins.job_detail did not include build {build_number}={result}: {output}"
    );
    Ok(())
}

async fn wait_for_build_result(
    config: &JenkinsConfig,
    job: &str,
    build_number: u64,
    result: &str,
) -> Result<CapabilityInvocationResult> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last_output = None;
    while Instant::now() <= deadline {
        let detail = invoke_checked(
            config,
            "job_detail",
            json!({ "job_full_name": job }),
            false,
            false,
            Some(Pagination {
                limit: 20,
                cursor: None,
            }),
            "jenkins.job_detail wait for triggered build",
        )
        .await?;
        if ensure_build_result(&detail.output, build_number, result).is_ok() {
            return Ok(detail);
        }
        last_output = Some(detail.output);
        sleep(Duration::from_secs(1)).await;
    }
    bail!(
        "timed out waiting for Jenkins build {build_number}={result}; last output: {}",
        last_output.unwrap_or(Value::Null)
    )
}

async fn ensure_missing_job_error_redacts(config: &JenkinsConfig, job: &str) -> Result<()> {
    let missing_job = format!("{job}-missing");
    match invoke(
        config,
        "job_detail",
        json!({ "job_full_name": missing_job }),
        false,
        false,
        None,
    )
    .await
    {
        Ok(result) => bail!(
            "expected jenkins.job_detail missing-job failure, got output: {}",
            result.output
        ),
        Err(error) => {
            ensure!(
                error.category == CapabilityErrorCategory::TargetSystem,
                "missing job should be a target error: {:?}",
                error.category
            );
            ensure_error_excludes_config(&error, config, "jenkins.job_detail missing job")?;
        }
    }
    Ok(())
}

async fn ensure_bad_auth_redacts(config: &JenkinsConfig) -> Result<()> {
    let bad = bad_auth_config(config);
    match invoke(&bad, "jobs", json!({}), false, false, None).await {
        Ok(result) => bail!(
            "expected jenkins.jobs bad-auth failure, got output: {}",
            result.output
        ),
        Err(error) => {
            ensure!(
                error.category == CapabilityErrorCategory::TargetSystem,
                "bad auth should be a target error: {:?}",
                error.category
            );
            ensure_error_excludes_config(&error, config, "jenkins.jobs bad auth")?;
            ensure_error_excludes_config(&error, &bad, "jenkins.jobs bad auth")?;
        }
    }
    Ok(())
}

async fn ensure_unavailable_target_redacts() -> Result<()> {
    let unavailable = unavailable_config();
    match invoke(&unavailable, "jobs", json!({}), false, false, None).await {
        Ok(result) => bail!(
            "expected jenkins.jobs unavailable failure, got output: {}",
            result.output
        ),
        Err(error) => {
            ensure!(
                error.category == CapabilityErrorCategory::TargetSystem,
                "unavailable target should be a target error: {:?}",
                error.category
            );
            ensure_error_excludes_config(&error, &unavailable, "jenkins.jobs unavailable")?;
            ensure!(
                error.redaction == RedactionStatus::Applied,
                "unavailable target should redact configured URL/auth material: {:?}",
                error.redaction
            );
        }
    }
    Ok(())
}

fn actor() -> ActorRef {
    ActorRef {
        id: "agent:jenkins-fixture-smoke".into(),
        actor_type: ActorType::Agent,
    }
}

fn acknowledgement() -> InvocationAcknowledgement {
    InvocationAcknowledgement {
        actor: actor(),
        acknowledged_at: Utc::now(),
        reason: Some("fixture smoke mutation scoped to generated Jenkins job".into()),
        approval_id: Some("jenkins-fixture-smoke".into()),
    }
}
