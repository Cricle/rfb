//! Drive the web bench: precondition gating, concurrent SSE turn execution,
//! and per-level resource sampling.

use crate::cli::error::{external, validation, CliError};
use crate::cli::forkd;
use crate::cli::localhost::{require_localhost, require_snapshot_tag};
use crate::cli::web_bench::aggregate::aggregate;
use crate::cli::web_bench::args::WebBenchArgs;
use crate::cli::web_bench::effective_interval;
use crate::cli::web_bench::types::{ResourceSample, Sample, Segments, StreamState};
use crate::cli::web_bench::util::{
    check_pid_alive, check_ready, config_api_key_env, default_report_path, git_head,
    now_utc_compact, parse_levels, sample_pid, write_report,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Run the full web bench and return the sanitized summary. Blocks (exit 3)
/// on any precondition the script used to treat as BLOCKED.
pub async fn bench(args: WebBenchArgs) -> Result<Value, CliError> {
    let base_url = require_localhost(&args.base_url)?;
    let forkd_url = require_localhost(&args.forkd_url)?;
    let tag = require_snapshot_tag(&args.tag)?;

    // Config must be readable and name a model-key environment variable. The
    // variable's value is never printed, parsed, or included in any report.
    let api_key_env = config_api_key_env(args.config.as_deref())?;

    if args.requests < 100 {
        return Err(validation(
            "requests must be at least 100 successful-sample capacity",
        ));
    }
    if args.warmup < 10 {
        return Err(validation("warmup must be at least 10"));
    }
    let levels = parse_levels(&args.levels)?;

    let key_present = std::env::var(&api_key_env)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !key_present {
        return Err(validation(format!(
            "required model key environment variable is absent: {api_key_env}"
        )));
    }

    check_pid_alive(args.pid)?;

    if args.require_vm {
        forkd::preflight(&forkd_url, tag, true).await?;
    }

    let ready_client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| external(format!("build readiness client: {error}")))?;
    check_ready(&ready_client, &base_url, "/readyz").await?;
    check_ready(&ready_client, &forkd_url, "/v1/snapshots").await?;

    let client = Client::builder()
        .build()
        .map_err(|error| external(format!("build http client: {error}")))?;

    let mut results = Vec::new();
    for &level in &levels {
        let result = run_level(&client, Arc::new(args.clone()), &base_url, level).await?;
        eprintln!(
            "web bench: concurrency={level} status={} success={} timeout={} rss_max={}",
            result.get("status").and_then(Value::as_str).unwrap_or("?"),
            result.get("success").and_then(Value::as_u64).unwrap_or(0),
            result.get("timeout").and_then(Value::as_u64).unwrap_or(0),
            result
                .get("resources")
                .and_then(|resource| resource.get("rss_bytes_max"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        results.push(result);
    }

    let overall = if results
        .iter()
        .all(|result| result.get("status").and_then(Value::as_str) == Some("PASS"))
    {
        "PASS"
    } else {
        "INSUFFICIENT_SAMPLE"
    };
    let summary = json!({
        "run_id": format!("rfb-web-bench-{}", now_utc_compact()),
        "status": overall,
        "levels": results,
        "metadata": {
            "head": git_head(),
            "worktree": "dirty_or_unknown",
            "duration_s": args.duration,
            "sample_interval_s": effective_interval(args.sample_interval),
            "tool_required": args.require_tool,
            "payloads": "suppressed",
            "secrets": "suppressed",
            "resource_scope": "RFB web service process only; controller/guest/sandbox metrics unavailable",
        },
    });

    let report = args.report.clone().unwrap_or_else(default_report_path);
    write_report(&report, &summary)?;
    Ok(summary)
}

/// One concurrency level: warmup (excluded), then measured turns with the
/// RFB web service PID sampled in the background.
async fn run_level(
    client: &Client,
    args: Arc<WebBenchArgs>,
    base_url: &str,
    level: u32,
) -> Result<Value, CliError> {
    let level = level as usize;
    let base_offset = level * 100_000;
    // Warmup is real traffic but excluded from measured aggregates.
    run_batch(
        client,
        Arc::clone(&args),
        base_url,
        level,
        args.warmup,
        base_offset,
    )
    .await?;

    let interval = Duration::from_secs_f64(effective_interval(args.sample_interval));
    let pid = args.pid;
    let (resource_tx, mut resource_rx) = tokio::sync::mpsc::channel::<ResourceSample>(512);
    let sampler = tokio::spawn(async move {
        loop {
            if let Some(sample) = sample_pid(pid) {
                if resource_tx.send(sample).await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(interval).await;
        }
    });

    let started = Instant::now();
    let samples = if args.duration > 0.0 {
        run_duration(
            client,
            Arc::clone(&args),
            base_url,
            level,
            base_offset + args.warmup,
            Duration::from_secs_f64(args.duration),
        )
        .await?
    } else {
        run_batch(
            client,
            Arc::clone(&args),
            base_url,
            level,
            args.requests,
            base_offset + args.warmup,
        )
        .await?
    };
    sampler.abort();
    let mut resources = Vec::new();
    while let Ok(sample) = resource_rx.try_recv() {
        resources.push(sample);
    }

    Ok(aggregate(
        args.warmup,
        level,
        started.elapsed().as_secs_f64(),
        &samples,
        &resources,
    ))
}

/// Run `count` turns with at most `level` in flight, collecting all samples.
async fn run_batch(
    client: &Client,
    args: Arc<WebBenchArgs>,
    base_url: &str,
    level: usize,
    count: usize,
    base_offset: usize,
) -> Result<Vec<Sample>, CliError> {
    let mut set = tokio::task::JoinSet::new();
    let mut samples = Vec::with_capacity(count);
    let mut next = 0usize;
    while next < count || !set.is_empty() {
        while set.len() < level && next < count {
            set.spawn(one(
                client.clone(),
                Arc::clone(&args),
                base_url.to_owned(),
                base_offset + next,
            ));
            next += 1;
        }
        if let Some(joined) = set.join_next().await {
            match joined {
                Ok(sample) => samples.push(sample),
                Err(error) => return Err(external(format!("web bench task panicked: {error}"))),
            }
        }
    }
    Ok(samples)
}

/// Keep firing turns until `duration` elapses, capped at `level` in flight.
async fn run_duration(
    client: &Client,
    args: Arc<WebBenchArgs>,
    base_url: &str,
    level: usize,
    start_n: usize,
    duration: Duration,
) -> Result<Vec<Sample>, CliError> {
    let mut set = tokio::task::JoinSet::new();
    let mut samples = Vec::new();
    let mut next = start_n;
    let deadline = Instant::now() + duration;
    loop {
        while set.len() < level {
            set.spawn(one(
                client.clone(),
                Arc::clone(&args),
                base_url.to_owned(),
                next,
            ));
            next += 1;
        }
        if Instant::now() >= deadline {
            break;
        }
        if let Some(joined) = set.join_next().await {
            match joined {
                Ok(sample) => samples.push(sample),
                Err(error) => return Err(external(format!("web bench task panicked: {error}"))),
            }
        }
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(sample) => samples.push(sample),
            Err(error) => return Err(external(format!("web bench task panicked: {error}"))),
        }
    }
    Ok(samples)
}

/// One SSE turn against `/api/ai/rfb/stream`. Never propagates: stream-level
/// errors/timeouts become `ok: false` samples, matching the script.
async fn one(client: Client, args: Arc<WebBenchArgs>, base_url: String, n: usize) -> Sample {
    let mut body = json!({ "prompt": args.prompt });
    if let Some(model) = &args.model {
        body["model"] = json!(model);
    }
    let start = Instant::now();
    let request = client
        .post(format!("{base_url}/api/ai/rfb/stream"))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("x-user-id", format!("rfb-bench-{n}"))
        .json(&body)
        .timeout(Duration::from_secs(args.timeout));
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return Sample::timed_out(),
        Err(_) => return Sample::default(),
    };
    if !response.status().is_success() {
        return Sample::default();
    }

    let mut state = StreamState::default();
    let mut event = String::new();
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                state.clean = true;
                break;
            }
            Err(error) if error.is_timeout() => {
                state.timeout = true;
                break;
            }
            Err(_) => {
                state.error = true;
                break;
            }
        };
        pending.extend_from_slice(&chunk);
        let mut processed = 0;
        while let Some(position) = pending[processed..].iter().position(|&byte| byte == b'\n') {
            let line = &pending[processed..processed + position];
            process_line(
                line,
                start.elapsed().as_nanos() as u64,
                &mut event,
                &mut state,
            );
            processed += position + 1;
        }
        pending.drain(..processed);
    }
    if !pending.is_empty() {
        process_line(
            &pending,
            start.elapsed().as_nanos() as u64,
            &mut event,
            &mut state,
        );
    }

    let complete = state
        .done
        .unwrap_or_else(|| start.elapsed().as_nanos() as u64);
    let tool_ok = if args.require_tool {
        state.tool_call && state.tool_result
    } else {
        true
    };
    // A stream that ends cleanly (server closes) is a completed turn even when
    // the last line carried no explicit `done` event, matching the script.
    let done = state.done.or_else(|| state.clean.then_some(complete));
    Sample {
        ok: done.is_some() && !state.error && !state.cancelled && tool_ok,
        ttfb: state.ttfb,
        complete: done,
        timeout: state.timeout,
        cancelled: state.cancelled,
        segments: Segments {
            provider: state.provider,
            sandbox: state.sandbox,
            tool: state.tool,
            sse: state.sse,
        },
    }
}

/// Parse one SSE line into segment timings and flags. The `done`/`error`
/// heuristics mirror the legacy Python sampler; `event` is reset only after a
/// successfully parsed `data:` line.
fn process_line(line: &[u8], now: u64, event: &mut String, state: &mut StreamState) {
    if state.ttfb.is_none() {
        state.ttfb = Some(now);
        state.sse = Some(now);
    }
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if let Some(name) = text.strip_prefix("event:") {
        *event = name.trim().to_owned();
        return;
    }
    if !text.starts_with("data:") {
        return;
    }
    let data = text[5..].trim();
    let value: Value = match serde_json::from_str(data) {
        Ok(value) => value,
        Err(_) => return,
    };
    let typ = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event)
        .to_ascii_lowercase();
    let item_type = value
        .get("item")
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if typ.contains("provider") || typ.contains("response") {
        state.provider.get_or_insert(now);
    }
    if typ.contains("session.started") || typ == "session" {
        state.sandbox.get_or_insert(now);
    }
    if typ.contains("tool") || item_type.contains("tool") {
        state.tool.get_or_insert(now);
        if typ.contains("call") || item_type.contains("call") {
            state.tool_call = true;
        }
        if typ.contains("result") || item_type.contains("result") {
            state.tool_result = true;
        }
    }
    if typ.contains("cancel") {
        state.cancelled = true;
    }
    if typ == "done" || typ == "turn.completed" || typ == "response.completed" || event == "done" {
        state.done.get_or_insert(now);
    }
    if typ == "error" || typ == "provider.error" || typ == "turn.failed" || typ == "response.failed"
    {
        state.error = true;
    }
    *event = String::new();
}
