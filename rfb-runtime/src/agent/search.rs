//! Structured filesystem RPCs (ls/find/grep/read/write/eval) and the bounded
//! directory walks that back them.

use super::transport::{guest_path, limit, pattern, MAX_BYTES, MAX_CODE, MAX_RESULTS};
use crate::agent::process_exec::execute;
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Seek, SeekFrom};
use std::path::Path;

/// Files larger than this are skipped by `grep` instead of being scanned, so
/// the walk's memory stays bounded regardless of workspace contents.
const GREP_SCAN_CAP: u64 = 16 * 1024 * 1024;

pub async fn structured(request: &Value) -> io::Result<Value> {
    match request.get("action").and_then(Value::as_str).unwrap_or("") {
        "ls" => {
            let path = guest_path(request.get("path"), true)?;
            let max = limit(request.get("max_results"), MAX_RESULTS, MAX_RESULTS)?;
            // Bounded collection: read at most 2*max+1 entries so a huge
            // directory cannot materialize millions of entries, while the
            // sort still sees a representative sample. `truncated` is set
            // whenever entries were dropped (early stop or the final cut).
            let scan_cap = max.saturating_mul(2).saturating_add(1);
            let mut entries = Vec::new();
            let mut truncated = false;
            for item in std::fs::read_dir(path)? {
                let item = item?;
                if entries.len() >= scan_cap {
                    truncated = true;
                    break;
                }
                let meta = item.metadata()?;
                entries.push(json!({"name":item.file_name().to_string_lossy(),"is_dir":meta.is_dir(),"size":if meta.is_file(){Some(meta.len())}else{None}}));
            }
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            if entries.len() > max {
                truncated = true;
                entries.truncate(max);
            }
            Ok(json!({"entries":entries,"truncated":truncated}))
        }
        "find" => {
            let root = guest_path(request.get("path"), true)?;
            let p = pattern(request)?;
            let max = limit(request.get("max_results"), MAX_RESULTS, MAX_RESULTS)?;
            let mut out = Vec::new();
            let truncated = find_walk(&root, &root, p, max, &mut out)?;
            Ok(json!({"matches":out,"truncated":truncated}))
        }
        "grep" => {
            let root = guest_path(request.get("path"), true)?;
            let p = pattern(request)?;
            let max = limit(request.get("max_results"), MAX_RESULTS, MAX_RESULTS)?;
            let bytes = limit(request.get("max_bytes"), MAX_BYTES, MAX_BYTES)?;
            let mut out = Vec::new();
            let truncated = grep_walk(&root, &root, p, max, bytes, &mut out)?;
            Ok(json!({"matches":out,"truncated":truncated}))
        }
        "read" => {
            let path = guest_path(request.get("path"), false)?;
            let max = limit(request.get("max_bytes"), MAX_BYTES, MAX_BYTES)?;
            let offset = request.get("offset").and_then(Value::as_u64).unwrap_or(0);
            // Bounded read: only `max` bytes starting at `offset` are
            // materialized, never the whole file.
            let mut file = std::fs::File::open(&path)?;
            let total = file.metadata()?.len();
            file.seek(SeekFrom::Start(offset))?;
            let mut data = Vec::new();
            file.take(max as u64).read_to_end(&mut data)?;
            let truncated = offset + (data.len() as u64) < total;
            Ok(json!({"data":data,"truncated":truncated,"total_bytes":total}))
        }
        "write" => {
            let path = guest_path(request.get("path"), false)?;
            let data = request
                .get("data")
                .or_else(|| request.get("content"))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "data is required"))?;
            let bytes: Vec<u8> = if let Some(s) = data.as_str() {
                s.as_bytes().to_vec()
            } else {
                serde_json::from_value(data.clone()).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "data must be bytes or string")
                })?
            };
            if bytes.len() > MAX_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "payload too large",
                ));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if request
                .get("append")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?
                    .write_all(&bytes)?;
            } else {
                std::fs::write(path, &bytes)?;
            }
            Ok(json!({"bytes_written":bytes.len()}))
        }
        "eval" => {
            let code = request.get("code").and_then(Value::as_str).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "code must be a string")
            })?;
            if code.trim().is_empty() || code.len() > MAX_CODE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid eval code",
                ));
            }
            let mut r = request.clone();
            r["args"] = json!(["/bin/sh", "-c", code]);
            if let Some(cwd) = request.get("cwd") {
                r["cwd"] = cwd.clone();
            }
            let result = Box::pin(execute(&r)).await?;
            // Eval has its own typed wire contract. Keep `exec`'s historical
            // out/err/exit_code fields intact, but expose output/status here.
            let output = result["out"]
                .as_str()
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            Ok(json!({
                "output": output,
                "status": result["exit_code"].clone(),
                "timed_out": result["timed_out"].clone(),
            }))
        }
        _ => unreachable!(),
    }
}

/// Returns whether the walk stopped early because the `max` cap was hit
/// (the caller reports it as `truncated`).
fn find_walk(
    root: &Path,
    dir: &Path,
    pat: &str,
    max: usize,
    out: &mut Vec<String>,
) -> io::Result<bool> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if out.len() >= max {
            return Ok(true);
        }
        let name = e.file_name().to_string_lossy().to_string();
        if if pat.contains('*') {
            glob_matches(pat, &name)
        } else {
            name.contains(pat)
        } {
            out.push(
                p.strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
        if e.file_type()?.is_dir() && find_walk(root, &p, pat, max, out)? {
            return Ok(true);
        }
    }
    Ok(out.len() >= max)
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    // Glob matching is intentionally small and deterministic: `*` matches any
    // sequence (including separators), while all other characters are literal.
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Returns whether the walk stopped early because a cap was hit (the caller
/// reports it as `truncated`).
fn grep_walk(
    root: &Path,
    dir: &Path,
    pat: &str,
    max: usize,
    bytes: usize,
    out: &mut Vec<Value>,
) -> io::Result<bool> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_dir() {
            if out.len() < max && grep_walk(root, &p, pat, max, bytes, out)? {
                return Ok(true);
            }
            continue;
        }
        // Bound the per-file cost: skip oversized files outright and stream
        // the rest line-by-line so a large file never materializes in memory.
        if std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) > GREP_SCAN_CAP {
            continue;
        }
        let Ok(file) = std::fs::File::open(&p) else {
            continue;
        };
        let mut reader = io::BufReader::with_capacity(64 * 1024, file);
        let needle = pat.as_bytes();
        let path = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let mut consumed = 0usize;
        let mut line_no = 0usize;
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => line_no += 1,
                Err(_) => break,
            }
            if line.windows(needle.len()).any(|w| w == needle) {
                let text = String::from_utf8_lossy(&line);
                let item = json!({"path":path,"line":line_no,"text":text});
                let item_bytes = serde_json::to_vec(&item).unwrap_or_default().len();
                if consumed + item_bytes > bytes && !out.is_empty() {
                    return Ok(true);
                }
                consumed += item_bytes;
                out.push(item);
                if out.len() >= max || consumed >= bytes {
                    return Ok(true);
                }
            }
        }
    }
    Ok(out.len() >= max)
}
