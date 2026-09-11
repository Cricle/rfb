//! Streaming process execution for the forkd agent: live stdout/stderr frames,
//! stdin injection, stop, and bounded deadlines.

use super::builtin::{builtin, builtin_result, validate_builtin_request};
use super::process_exec::{command_from, prepare_process, terminate};
use super::MAX_LINE;
use crate::agent::write_json;
use serde_json::{json, Value};
use std::io;
use std::time::Duration;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

pub async fn stream_process<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    request: &Value,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<()> {
    if request.get("pty").and_then(Value::as_bool).unwrap_or(false) {
        // Explicit rejection: the Rust replacement does not implement PTY, so
        // it must not silently degrade a PTY request into plain pipes.
        write_json(
            writer,
            json!({"error":"pty is not supported","exit_code":1}),
        )
        .await?;
        return Ok(());
    }
    // Establish the deadline before spawning so the complete request, not just
    // child execution after spawn, is bounded by the caller's timeout.
    let deadline = request
        .get("timeout")
        .and_then(Value::as_u64)
        .map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));
    if let Some(kind) = builtin(request) {
        // Keep the same request validation and wire lifecycle without creating
        // a process (shell-free rootfs images may lack /bin/echo).
        validate_builtin_request(request)?;
        let result = builtin_result(request, kind);
        write_json(writer, json!({"stream":"started","pid":null,"pty":false})).await?;
        if !result["out"].as_str().unwrap_or_default().is_empty() {
            write_json(writer, json!({"out":result["out"].clone()})).await?;
        }
        write_json(
            writer,
            json!({"exit_code":result["exit_code"].clone(),"timed_out":false}),
        )
        .await?;
        return Ok(());
    }
    let mut command = command_from(request)?;
    prepare_process(&mut command, true);
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing stderr"))?;
    write_json(
        writer,
        json!({"stream":"started","pid":child.id(),"pty":false}),
    )
    .await?;
    let mut out = BufReader::new(stdout);
    let mut err = BufReader::new(stderr);
    let mut ob = vec![0u8; 8192];
    let mut eb = vec![0u8; 8192];
    let mut input = Vec::new();
    // Keep the timeout branch pending when no timeout was requested.  Do not
    // substitute an arbitrary multi-year duration: a request timeout is a
    // deadline, and its clock starts before spawning the child.
    let timeout_sleep = async move {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timeout_sleep);
    loop {
        tokio::select! {
            _ = &mut timeout_sleep => {
                terminate(&mut child).await;
                // Reap the child, drain both pipes to EOF, then report the
                // terminal frame. The process-group kill above also covers
                // descendants that inherited the pipes.
                let _ = child.wait().await;
                drain_streams(&mut out, &mut err, &mut ob, &mut eb, writer).await?;
                // `done` is the host's terminal marker for a null exit_code;
                // without it a null exit_code frame fails to decode on the
                // host. No `err`/`error` key here: the host would deliver a
                // string `error` as a fatal Remote failure instead of the
                // timeout terminal.
                write_json(writer, json!({"exit_code":null,"timed_out":true,"done":true})).await?;
                return Ok(());
            }
            status=child.wait()=>{
                let status = status?;
                // A fast child can exit before either pipe-read branch wins
                // the select. Always drain both pipes before the terminal
                // frame so output and exit status cannot race on the wire.
                drain_streams(&mut out, &mut err, &mut ob, &mut eb, writer).await?;
                write_json(writer,json!({"exit_code":status.code(),"timed_out":false})).await?;
                return Ok(())
            }
            n=out.read(&mut ob)=>{
                let n=n?;
                if n>0 { write_json(writer,json!({"out":String::from_utf8_lossy(&ob[..n])})).await?; }
            }
            n=err.read(&mut eb)=>{
                let n=n?;
                if n>0 { write_json(writer,json!({"err":String::from_utf8_lossy(&eb[..n])})).await?; }
            }
            outcome = read_line_bounded(reader, &mut input, MAX_LINE) => {
                match outcome? {
                    LineReadOutcome::Eof => {
                        terminate(&mut child).await;
                        let _ = child.wait().await;
                        drain_streams(&mut out, &mut err, &mut ob, &mut eb, writer).await?;
                        // A client-side stdin EOF ends the session with a clean
                        // terminal frame (no `error` key: the host turns any
                        // string `error` into a fatal Remote failure).
                        write_json(writer, json!({"exit_code":null,"timed_out":false,"done":true})).await?;
                        return Ok(());
                    }
                    LineReadOutcome::TooLong => {
                        // Oversized input line: kill the child and still emit
                        // the terminal frame so the host session cannot hang
                        // until its read timeout.
                        terminate(&mut child).await;
                        let _ = child.wait().await;
                        drain_streams(&mut out, &mut err, &mut ob, &mut eb, writer).await?;
                        write_json(writer, json!({"exit_code":null,"timed_out":false,"done":true})).await?;
                        return Ok(());
                    }
                    LineReadOutcome::Line => {}
                }
                let v:Value=serde_json::from_slice(input.trim_ascii()).unwrap_or(Value::Null);
                if v.get("action").and_then(Value::as_str)==Some("stop") {
                    terminate(&mut child).await;
                    let _ = child.wait().await;
                    drain_streams(&mut out, &mut err, &mut ob, &mut eb, writer).await?;
                    // `done` lets the host decode this null-exit_code terminal
                    // frame instead of failing with "invalid guest stream
                    // event".
                    write_json(writer, json!({"exit_code":null,"timed_out":false,"done":true})).await?;
                    return Ok(());
                } else if let Some(text)=v.get("in").and_then(Value::as_str) {
                    // Bound the forward: a child that stops draining its stdin
                    // must not make the stop/timeout paths unreachable (the
                    // select cannot poll other branches while this arm body is
                    // awaiting a full pipe).
                    let write = async {
                        stdin.write_all(text.as_bytes()).await?;
                        stdin.flush().await
                    };
                    match tokio::time::timeout(STDIN_FORWARD_TIMEOUT, write).await {
                        Ok(Ok(())) => {}
                        // Broken pipe: the child is gone; the wait branch
                        // reaps it and emits the real terminal frame.
                        Ok(Err(error)) => {
                            eprintln!("rfb-agent: stream stdin write failed: {error}");
                        }
                        Err(_) => {
                            terminate(&mut child).await;
                            let _ = child.wait().await;
                            drain_streams(&mut out, &mut err, &mut ob, &mut eb, writer).await?;
                            write_json(writer, json!({"exit_code":null,"timed_out":false,"done":true})).await?;
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// How long a single stdin forward may block before the session is
/// force-terminated. Bounds how long a full child pipe can delay the
/// stop/timeout paths (the select cannot poll other branches while this arm
/// body is awaiting a write).
const STDIN_FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

enum LineReadOutcome {
    /// A complete line (or a trailing partial line at EOF) sits in the buffer.
    Line,
    /// Clean end of stream with no buffered bytes.
    Eof,
    /// The line exceeded the cap; no further bytes were buffered.
    TooLong,
}

/// Bounded line read: never buffers more than `cap` bytes of one line, unlike
/// `AsyncBufReadExt::read_until` which grows the buffer until a newline
/// arrives (a newline-free flood would otherwise OOM the agent).
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
    cap: usize,
) -> io::Result<LineReadOutcome> {
    out.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if out.is_empty() {
                LineReadOutcome::Eof
            } else {
                LineReadOutcome::Line
            });
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(position) => {
                if out.len() + position + 1 > cap {
                    return Ok(LineReadOutcome::TooLong);
                }
                out.extend_from_slice(&available[..=position]);
                reader.consume(position + 1);
                return Ok(LineReadOutcome::Line);
            }
            None => {
                if out.len() + available.len() > cap {
                    return Ok(LineReadOutcome::TooLong);
                }
                let take = available.len();
                out.extend_from_slice(available);
                reader.consume(take);
            }
        }
    }
}

async fn drain_streams<R1: AsyncRead + Unpin, R2: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    out: &mut R1,
    err: &mut R2,
    ob: &mut [u8],
    eb: &mut [u8],
    writer: &mut W,
) -> io::Result<()> {
    // Drain stdout and stderr concurrently. Reading one pipe to EOF before
    // touching the other can deadlock when a child fills stderr (or stdout)
    // while the other pipe remains open. This path runs after wait(), so both
    // pipes must be consumed independently before the terminal frame.
    let mut out_open = true;
    let mut err_open = true;
    while out_open || err_open {
        tokio::select! {
            result = out.read(ob), if out_open => {
                let n = result?;
                if n == 0 {
                    out_open = false;
                } else {
                    write_json(writer, json!({"out":String::from_utf8_lossy(&ob[..n])})).await?;
                }
            }
            result = err.read(eb), if err_open => {
                let n = result?;
                if n == 0 {
                    err_open = false;
                } else {
                    write_json(writer, json!({"err":String::from_utf8_lossy(&eb[..n])})).await?;
                }
            }
        }
    }
    Ok(())
}
