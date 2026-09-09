//! Structured filesystem RPCs (ls/find/grep/read/write/eval) and the bounded
//! directory walks that back them.

use super::transport::{guest_path, limit, pattern, MAX_BYTES, MAX_CODE, MAX_RESULTS};
use crate::agent::process_exec::execute;
use serde_json::{json, Value};
use std::io;
use std::path::Path;

pub async fn structured(request: &Value) -> io::Result<Value> {
    match request.get("action").and_then(Value::as_str).unwrap_or("") {
        "ls" => {
            let path = guest_path(request.get("path"), true)?;
            let max = limit(request.get("max_results"), MAX_RESULTS, MAX_RESULTS)?;
            let mut entries = Vec::new();
            for item in std::fs::read_dir(path)? {
                let item = item?;
                let meta = item.metadata()?;
                entries.push(json!({"name":item.file_name().to_string_lossy(),"is_dir":meta.is_dir(),"size":if meta.is_file(){Some(meta.len())}else{None}}));
            }
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            let truncated = entries.len() > max;
            entries.truncate(max);
            Ok(json!({"entries":entries,"truncated":truncated}))
        }
        "find" => {
            let root = guest_path(request.get("path"), true)?;
            let p = pattern(request)?;
            let max = limit(request.get("max_results"), MAX_RESULTS, MAX_RESULTS)?;
            let mut out = Vec::new();
            find_walk(&root, &root, p, max, &mut out)?;
            let truncated = out.len() >= max;
            out.truncate(max);
            Ok(json!({"matches":out,"truncated":truncated}))
        }
        "grep" => {
            let root = guest_path(request.get("path"), true)?;
            let p = pattern(request)?;
            let max = limit(request.get("max_results"), MAX_RESULTS, MAX_RESULTS)?;
            let bytes = limit(request.get("max_bytes"), MAX_BYTES, MAX_BYTES)?;
            let mut out = Vec::new();
            grep_walk(&root, &root, p, max, bytes, &mut out)?;
            let truncated = out.len() > max;
            out.truncate(max);
            Ok(json!({"matches":out,"truncated":truncated}))
        }
        "read" => {
            let path = guest_path(request.get("path"), false)?;
            let max = limit(request.get("max_bytes"), MAX_BYTES, MAX_BYTES)?;
            let offset = request.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let data = std::fs::read(path)?;
            let start = (offset as usize).min(data.len());
            let end = (start + max).min(data.len());
            Ok(
                json!({"data":data[start..end].to_vec(),"truncated":end<data.len(),"total_bytes":data.len()}),
            )
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
            let result = execute(&r).await?;
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

fn find_walk(
    root: &Path,
    dir: &Path,
    pat: &str,
    max: usize,
    out: &mut Vec<String>,
) -> io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
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
        if e.file_type()?.is_dir() && out.len() <= max {
            find_walk(root, &p, pat, max, out)?;
        }
    }
    Ok(())
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

fn grep_walk(
    root: &Path,
    dir: &Path,
    pat: &str,
    max: usize,
    bytes: usize,
    out: &mut Vec<Value>,
) -> io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_dir() {
            if out.len() <= max {
                grep_walk(root, &p, pat, max, bytes, out)?;
            }
        } else if let Ok(data) = std::fs::read(&p) {
            // Search the raw bytes, not only valid UTF-8 text. Split on LF so
            // binary files and invalid UTF-8 still produce useful line hits;
            // lossy conversion is only used for the textual wire field.
            let needle = pat.as_bytes();
            let path = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let mut consumed = 0usize;
            for (line_no, line) in (1usize..).zip(data.split(|b| *b == b'\n')) {
                if line.windows(needle.len()).any(|w| w == needle) {
                    let text = String::from_utf8_lossy(line);
                    let item = json!({"path":path,"line":line_no,"text":text});
                    let item_bytes = serde_json::to_vec(&item).unwrap_or_default().len();
                    if consumed + item_bytes > bytes && !out.is_empty() {
                        return Ok(());
                    }
                    consumed += item_bytes;
                    out.push(item);
                    if out.len() >= max || consumed >= bytes {
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}
