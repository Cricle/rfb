use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

#[derive(Debug, thiserror::Error)]
/// Errors returned by the forkd controller client.
pub enum ForkdClientError {
    /// The controller request failed at the transport level.
    #[error("forkd request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// The controller returned a non-success HTTP status.
    #[error("forkd returned {status}: {message}")]
    Http {
        /// HTTP status code returned by the controller.
        status: StatusCode,
        /// Human-readable error message from the controller body.
        message: String,
    },
    /// The controller response could not be decoded or failed validation.
    #[error("invalid forkd response: {0}")]
    Decode(String),
}

#[derive(Clone)]
/// HTTP client for the forkd controller.
pub struct ForkdClient {
    client: Client,
    base_url: String,
    token: Option<String>,
}

/// Percent-encode one URL path segment (PROTOCOL.md §1.1: snapshot tags are
/// encoded verbatim into the path). Mirrors `urllib.parse.quote(tag, safe="")`
/// in the Python SDK: unreserved characters pass through, everything else
/// (including `/`, `?`, `#`, space, `%`) is escaped. A literal `.`/`..` tag is
/// still normalized away by the WHATWG URL parser, exactly as in the other
/// SDKs.
fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Metadata describing a forkd snapshot.
pub struct SnapshotInfo {
    /// Snapshot identifier.
    pub tag: String,
    /// Directory the snapshot is stored in, as reported by the controller.
    #[serde(default)]
    pub dir: String,
    /// Unix timestamp when the snapshot was created.
    #[serde(default)]
    pub created_at_unix: Option<i64>,
    /// Tag of the snapshot this snapshot was branched from, if any.
    #[serde(default)]
    pub branched_from: Option<String>,
    /// Time spent paused while capturing the snapshot, in milliseconds.
    #[serde(default)]
    pub pause_ms: Option<u64>,
    /// Time taken to compute the snapshot diff, in milliseconds.
    #[serde(default)]
    pub diff_ms: Option<u64>,
    /// Physical size of the snapshot diff in bytes.
    #[serde(default)]
    pub diff_physical_bytes: Option<u64>,
    /// Logical size of the snapshot diff in bytes.
    #[serde(default)]
    pub diff_logical_bytes: Option<u64>,
    /// Optional warning emitted while creating the snapshot.
    #[serde(default)]
    pub warning: Option<String>,
    /// Snapshot lifecycle status, e.g. `ready`.
    #[serde(default)]
    pub status: String,
    /// Whether the snapshot can be booted as a sandbox.
    #[serde(default)]
    pub bootable: bool,
    /// Optional controller-provided snapshot digest. Older controllers omit it.
    #[serde(default)]
    pub digest: Option<String>,
    /// Optional controller provenance/detail record. Kept opaque for compatibility.
    #[serde(default)]
    pub provenance: Option<serde_json::Value>,
}

/// Metadata describing a live forkd sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    /// Sandbox identifier.
    pub id: String,
    /// Snapshot tag the sandbox was created from.
    pub snapshot_tag: String,
    /// Optional network namespace the sandbox runs in.
    #[serde(default)]
    pub netns: Option<String>,
    /// Unix timestamp when the sandbox was created.
    #[serde(default)]
    pub created_at_unix: Option<i64>,
    /// TCP address of the sandbox guest.
    #[serde(default)]
    pub guest_addr: String,
    /// Optional memory limit in MiB.
    #[serde(default)]
    pub memory_limit_mib: Option<u64>,
    /// Optional host PID of the sandbox process.
    #[serde(default)]
    pub pid: Option<u32>,
    /// Whether the sandbox was created by branching a live guest.
    #[serde(default)]
    pub has_branched: bool,
    /// Number of branches recorded for this sandbox.
    #[serde(default)]
    pub branch_count: u32,
}

/// Parameters for creating one or more forkd sandboxes.
#[derive(Debug, Clone, Serialize)]
pub struct CreateSandboxRequest<'a> {
    /// Snapshot to use for the sandbox.
    pub snapshot_tag: &'a str,
    /// Number of sandboxes to create.
    pub n: usize,
    /// Give each created sandbox its own network namespace.
    pub per_child_netns: bool,
    /// Optional memory limit in MiB.
    pub memory_limit_mib: Option<u64>,
    /// Prewarm the sandbox before returning.
    pub prewarm: bool,
    /// Branch the sandbox from a live guest.
    pub live_fork: bool,
    /// Use huge pages for sandbox memory.
    pub hugepages: bool,
}

impl ForkdClient {
    pub fn validate_sandbox_id(id: &str) -> Result<(), ForkdClientError> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ForkdClientError::Decode("invalid forkd sandbox id".into()));
        }
        Ok(())
    }

    pub fn validate_guest_address(address: &str) -> Result<(), ForkdClientError> {
        let valid = address.parse::<std::net::SocketAddr>().is_ok();
        if !valid {
            return Err(ForkdClientError::Decode(
                "invalid forkd guest address".into(),
            ));
        }
        Ok(())
    }

    pub fn new(
        base_url: impl Into<String>,
        token: Option<String>,
        timeout: Duration,
    ) -> Result<Self, ForkdClientError> {
        let base_url = base_url.into();
        let parsed = Url::parse(&base_url).map_err(|e| ForkdClientError::Decode(e.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(ForkdClientError::Decode(
                "forkd URL must include http(s) scheme and host".into(),
            ));
        }
        let client = Client::builder()
            .timeout(timeout)
            // The controller is a loopback-only service: following redirects
            // would let it bounce requests to arbitrary hosts (and 301/302
            // would silently rewrite POST into GET).
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            // A blank token must not produce `Authorization: Bearer `
            // (UNIFIED_API.md §2: the header is sent only for a non-empty token).
            token: token.filter(|value| !value.trim().is_empty()),
        })
    }

    pub fn from_env() -> Result<Self, ForkdClientError> {
        let url = std::env::var("FORKD_URL").unwrap_or_else(|_| "http://127.0.0.1:8889".into());
        let token = std::env::var("FORKD_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty());
        Self::new(url, token, Duration::from_secs(10))
    }

    fn request(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Read a response body with a hard size cap: the client is loopback-only
    /// in normal use, but a broken/hostile controller must not OOM the CLI.
    async fn read_body_capped(mut response: reqwest::Response) -> Result<String, ForkdClientError> {
        const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
        if let Some(length) = response.content_length() {
            if length > MAX_BODY_BYTES as u64 {
                return Err(ForkdClientError::Decode(
                    "controller response too large".into(),
                ));
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(ForkdClientError::Decode(
                    "controller response too large".into(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn parse<T: for<'de> Deserialize<'de>>(
        response: reqwest::Response,
    ) -> Result<T, ForkdClientError> {
        let status = response.status();
        let body = Self::read_body_capped(response).await?;
        if !status.is_success() {
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
                .unwrap_or_else(|| body.chars().take(1024).collect());
            return Err(ForkdClientError::Http { status, message });
        }
        serde_json::from_str(&body).map_err(|e| ForkdClientError::Decode(e.to_string()))
    }

    pub async fn list_snapshots(&self) -> Result<Vec<SnapshotInfo>, ForkdClientError> {
        let r = self
            .request(self.client.get(format!("{}/v1/snapshots", self.base_url)))
            .send()
            .await?;
        Self::parse(r).await
    }

    /// Fetch detailed snapshot metadata from the current controller endpoint.
    ///
    /// Older controllers exposed the detail response at `/v1/snapshots/{tag}`;
    /// retry that path only when the preferred `/info` endpoint returns 404.
    /// A 404 from both endpoints is treated as unsupported detail and returns
    /// `None`.
    pub async fn snapshot_info(&self, tag: &str) -> Result<Option<SnapshotInfo>, ForkdClientError> {
        let tag = encode_path_segment(tag);
        let preferred = self
            .request(
                self.client
                    .get(format!("{}/v1/snapshots/{tag}/info", self.base_url)),
            )
            .send()
            .await?;
        if preferred.status() != StatusCode::NOT_FOUND {
            return Self::parse(preferred).await.map(Some);
        }

        let legacy = self
            .request(
                self.client
                    .get(format!("{}/v1/snapshots/{tag}", self.base_url)),
            )
            .send()
            .await?;
        if legacy.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::parse(legacy).await.map(Some)
    }

    pub async fn snapshot_ready(&self, tag: &str) -> Result<bool, ForkdClientError> {
        let snapshots = self.list_snapshots().await?;
        Ok(snapshots
            .iter()
            .any(|s| s.tag == tag && s.status.eq_ignore_ascii_case("ready") && s.bootable))
    }

    pub async fn wait_for_snapshot_ready(
        &self,
        tag: &str,
        timeout: Duration,
    ) -> Result<(), ForkdClientError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let snapshots = self.list_snapshots().await?;
            if let Some(snapshot) = snapshots.iter().find(|s| s.tag == tag) {
                if snapshot.status.eq_ignore_ascii_case("failed") {
                    return Err(ForkdClientError::Decode(format!(
                        "forkd snapshot `{tag}` is Failed"
                    )));
                }
                if snapshot.status.eq_ignore_ascii_case("ready") && snapshot.bootable {
                    return Ok(());
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ForkdClientError::Decode(format!(
                    "forkd snapshot `{tag}` did not become Ready before timeout"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100).min(deadline - now)).await;
        }
    }

    pub async fn create_sandbox(
        &self,
        request: &CreateSandboxRequest<'_>,
    ) -> Result<Vec<SandboxInfo>, ForkdClientError> {
        let r = self
            .request(
                self.client
                    .post(format!("{}/v1/sandboxes", self.base_url))
                    .json(request),
            )
            .send()
            .await?;
        Self::parse(r).await
    }

    /// List the live sandbox pool. Used for destroy reconciliation and orphan
    /// detection after errors.
    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxInfo>, ForkdClientError> {
        let r = self
            .request(self.client.get(format!("{}/v1/sandboxes", self.base_url)))
            .send()
            .await?;
        Self::parse(r).await
    }

    pub async fn ping(&self, sandbox_id: &str) -> Result<serde_json::Value, ForkdClientError> {
        Self::validate_sandbox_id(sandbox_id)?;
        let r = self
            .request(self.client.post(format!(
                "{}/v1/sandboxes/{}/ping",
                self.base_url, sandbox_id
            )))
            .send()
            .await?;
        Self::parse(r).await
    }

    pub async fn delete_sandbox(&self, sandbox_id: &str) -> Result<(), ForkdClientError> {
        Self::validate_sandbox_id(sandbox_id)?;
        let r = self
            .request(
                self.client
                    .delete(format!("{}/v1/sandboxes/{}", self.base_url, sandbox_id)),
            )
            .send()
            .await?;
        if r.status() == StatusCode::NOT_FOUND || r.status().is_success() {
            return Ok(());
        }
        Self::parse::<serde_json::Value>(r).await.map(|_| ())
    }
}
