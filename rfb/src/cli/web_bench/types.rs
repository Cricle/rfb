//! Internal sample/state/resource types for the web bench.

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Segments {
    pub(crate) provider: Option<u64>,
    pub(crate) sandbox: Option<u64>,
    pub(crate) tool: Option<u64>,
    pub(crate) sse: Option<u64>,
}

#[derive(Debug, Default)]
pub(crate) struct Sample {
    pub(crate) ok: bool,
    pub(crate) ttfb: Option<u64>,
    pub(crate) complete: Option<u64>,
    pub(crate) timeout: bool,
    pub(crate) cancelled: bool,
    pub(crate) segments: Segments,
}

impl Sample {
    pub(crate) fn timed_out() -> Self {
        Self {
            timeout: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct StreamState {
    pub(crate) ttfb: Option<u64>,
    pub(crate) provider: Option<u64>,
    pub(crate) sandbox: Option<u64>,
    pub(crate) tool: Option<u64>,
    pub(crate) sse: Option<u64>,
    pub(crate) done: Option<u64>,
    pub(crate) clean: bool,
    pub(crate) error: bool,
    pub(crate) cancelled: bool,
    pub(crate) tool_call: bool,
    pub(crate) tool_result: bool,
    pub(crate) timeout: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResourceSample {
    pub(crate) cpu_ticks: u64,
    pub(crate) rss_bytes: u64,
    pub(crate) fd_count: usize,
}
