//! Shared helpers: level/config validation, readiness probes, /proc sampling,
//! default paths, report writing, git head, and UTC timestamp formatting.

use crate::cli::error::{external, io, validation, CliError};
use crate::cli::web_bench::types::ResourceSample;
use reqwest::Client;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn parse_levels(text: &str) -> Result<Vec<u32>, CliError> {
    let mut levels = Vec::new();
    for part in text.split(',') {
        let level = part
            .trim()
            .parse::<u32>()
            .map_err(|_| validation("levels must be a comma-separated list of integers"))?;
        if !matches!(level, 1 | 2 | 4 | 8 | 16) {
            return Err(validation(format!(
                "unsupported concurrency level: {level}"
            )));
        }
        levels.push(level);
    }
    if levels.is_empty() {
        return Err(validation("levels is empty"));
    }
    Ok(levels)
}

/// Read the model-key env-var name from `rig.api_key_env` in the config.
pub(crate) fn config_api_key_env(config: Option<&Path>) -> Result<String, CliError> {
    let path = match config {
        Some(path) => path.to_path_buf(),
        None => default_config_path(),
    };
    let contents = fs::read_to_string(&path).map_err(|error| {
        validation(format!(
            "config is not readable: {} ({error})",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_str(&contents)
        .map_err(|_| validation(format!("config JSON is invalid: {}", path.display())))?;
    let key = value
        .get("rig")
        .and_then(|rig| rig.get("api_key_env"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if key.is_empty() {
        return Err(validation("rig.api_key_env is not configured"));
    }
    Ok(key.to_owned())
}

pub(crate) fn check_pid_alive(pid: u32) -> Result<(), CliError> {
    if pid == 0 {
        return Err(validation("PID must be a positive integer"));
    }
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return Err(validation(format!(
            "RFB web service PID is not alive: {pid}"
        )));
    }
    Ok(())
}

pub(crate) async fn check_ready(client: &Client, base: &str, path: &str) -> Result<(), CliError> {
    let response = client
        .get(format!("{base}{path}"))
        .header("accept", "application/json")
        .header("connection", "close")
        .send()
        .await
        .map_err(|error| external(format!("readiness probe {path} failed: {error}")))?;
    if response.status().as_u16() == 200 {
        Ok(())
    } else {
        Err(validation(format!(
            "readiness probe {path} returned {}",
            response.status()
        )))
    }
}

/// Sample CPU ticks (utime+stime), RSS bytes, and open fd count of `pid`.
pub(crate) fn sample_pid(pid: u32) -> Option<ResourceSample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let utime: u64 = fields.get(13)?.parse().ok()?;
    let stime: u64 = fields.get(14)?.parse().ok()?;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_kib: u64 = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())?;
    let fd_count = fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count();
    Some(ResourceSample {
        cpu_ticks: utime + stime,
        rss_bytes: rss_kib * 1024,
        fd_count,
    })
}

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

pub(crate) fn default_config_path() -> PathBuf {
    workspace_root().join("rfb-web").join("config.json")
}

pub(crate) fn default_report_path() -> PathBuf {
    workspace_root()
        .join("requirements")
        .join("RFB")
        .join("0.1.0")
        .join("performance-real-summary.json")
}

pub(crate) fn write_report(report: &Path, summary: &Value) -> Result<(), CliError> {
    if let Some(parent) = report.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            io(format!(
                "create report directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut text = serde_json::to_string_pretty(summary).map_err(|error| io(error.to_string()))?;
    text.push('\n');
    fs::write(report, text)
        .map_err(|error| io(format!("write report {}: {error}", report.display())))
}

pub(crate) fn git_head() -> String {
    let root = workspace_root();
    Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap_or("."),
            "rev-parse",
            "--short=12",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

pub(crate) fn now_utc_compact() -> String {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}

/// Convert days since 1970-01-01 to a civil (year, month, day) triple.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}
