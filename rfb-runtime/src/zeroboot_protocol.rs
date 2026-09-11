//! Strict ZBRT v1 full wire protocol primitives shared by host, guest, and
//! Firecracker integrations.
//!
//! Every frame starts with `ZBRT`, version 1, kind, flags, a 128-bit request
//! identifier, and a big-endian payload length. Payload codecs reject both
//! truncation and trailing bytes.
//!
//! This module is the canonical home for the ZeroBoot V1 wire contract. The
//! host/provider types in the `rfb` crate re-export these items
//! (`rfb::protocol`) so both crates share a single implementation without a
//! `rfb` <-> `rfb-runtime` dependency cycle.
#![allow(missing_docs, clippy::missing_errors_doc)]
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one complete ZBRT frame from an async transport, bounded by
/// [`MAX_PAYLOAD`].
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn read_frame_async<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Frame> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).await?;
    let n = u32::from_be_bytes(header[24..28].try_into().unwrap()) as usize;
    if n > MAX_PAYLOAD {
        return Err(err("payload too large"));
    };
    let mut bytes = Vec::with_capacity(HEADER_LEN + n);
    bytes.extend_from_slice(&header);
    let mut payload = vec![0; n];
    reader.read_exact(&mut payload).await?;
    bytes.extend_from_slice(&payload);
    Frame::decode(&mut bytes.as_slice())
}

/// Write one complete ZBRT frame to an async transport.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
pub async fn write_frame_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    frame.encode(&mut bytes)?;
    writer.write_all(&bytes).await
}

pub const MAGIC: [u8; 4] = *b"ZBRT";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 28;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// ZeroBoot V1 wire capability vocabulary — the single source of truth shared
/// by the host Hello (`rfb::zeroboot`), the guest HelloAck
/// (`zeroboot_connection`), the rootfs `/etc/zeroboot-capabilities` marker
/// written by `rfb-cli image build-rootfs`, and `rfb-cli zeroboot verify`.
pub const ZBRT_V1_CAPABILITIES: &[&str] = &[
    "execute",
    "stream",
    "deadline",
    "health",
    "cancel",
    "filesystem",
];
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelloAck {
    pub server: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deadline {
    pub timeout: Duration,
}

#[derive(Default)]
pub struct HostSession {
    hello: Option<Hello>,
    pub requests: HashMap<[u8; 16], Execute>,
    pub next_sequence: u64,
}
impl HostSession {
    pub fn new() -> Self {
        Self::default()
    }
    /// Negotiate the intersection of host and guest capabilities.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the operation fails; the error type carries the cause.
    pub fn negotiate_capabilities(
        &mut self,
        hello: Hello,
        supported: &[&str],
    ) -> io::Result<HelloAck> {
        if hello.client.trim().is_empty() {
            return Err(err("client is required"));
        }
        let capabilities = hello
            .capabilities
            .iter()
            .filter(|cap| supported.iter().any(|s| s == &cap.as_str()))
            .cloned()
            .collect();
        self.hello = Some(hello);
        Ok(HelloAck {
            server: "rfb-host".into(),
            capabilities,
        })
    }
    pub fn negotiate(&mut self, hello: Hello) -> io::Result<HelloAck> {
        self.negotiate_capabilities(hello, ZBRT_V1_CAPABILITIES)
    }
    pub fn submit(&mut self, id: [u8; 16], request: Execute) -> io::Result<()> {
        if self.hello.is_none() {
            return Err(err("hello negotiation required"));
        }
        if self.requests.insert(id, request).is_some() {
            return Err(err("duplicate request id"));
        }
        Ok(())
    }
    pub fn cancel(&mut self, id: &[u8; 16]) -> bool {
        self.requests.remove(id).is_some()
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Hello = 1,
    HelloAck = 2,
    Execute = 3,
    Output = 4,
    Exit = 5,
    Cancel = 6,
    CancelAck = 7,
    Fs = 8,
    FsResult = 9,
    Health = 10,
    HealthAck = 11,
    Error = 12,
    Result = 13,
}
impl Kind {
    fn parse(v: u8) -> io::Result<Self> {
        match v {
            1 => Ok(Self::Hello),
            2 => Ok(Self::HelloAck),
            3 => Ok(Self::Execute),
            4 => Ok(Self::Output),
            5 => Ok(Self::Exit),
            6 => Ok(Self::Cancel),
            7 => Ok(Self::CancelAck),
            8 => Ok(Self::Fs),
            9 => Ok(Self::FsResult),
            10 => Ok(Self::Health),
            11 => Ok(Self::HealthAck),
            12 => Ok(Self::Error),
            13 => Ok(Self::Result),
            _ => Err(err("unknown frame kind")),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    pub kind: Kind,
    pub flags: u16,
    pub request_id: [u8; 16],
    pub payload: Vec<u8>,
}
impl Frame {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        if self.flags != 0 || self.payload.len() > MAX_PAYLOAD {
            return Err(err("invalid frame"));
        };
        w.write_all(&MAGIC)?;
        w.write_all(&[VERSION, self.kind as u8])?;
        w.write_all(&self.flags.to_be_bytes())?;
        w.write_all(&self.request_id)?;
        w.write_all(&(self.payload.len() as u32).to_be_bytes())?;
        w.write_all(&self.payload)
    }
    pub fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut h = [0; HEADER_LEN];
        r.read_exact(&mut h)?;
        if h[..4] != MAGIC || h[4] != VERSION {
            return Err(err("invalid magic or version"));
        };
        let flags = u16::from_be_bytes([h[6], h[7]]);
        if flags != 0 {
            return Err(err("unsupported frame flags"));
        }
        let n = u32::from_be_bytes(h[24..28].try_into().unwrap()) as usize;
        if n > MAX_PAYLOAD {
            return Err(err("payload too large"));
        };
        let mut p = vec![0; n];
        r.read_exact(&mut p)?;
        Ok(Self {
            kind: Kind::parse(h[5])?,
            flags,
            request_id: h[8..24].try_into().unwrap(),
            payload: p,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hello {
    pub client: String,
    pub capabilities: Vec<String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Execute {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub stdin: Vec<u8>,
    pub timeout_ms: u32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub stream: u8,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exit {
    pub code: i32,
    pub signal: Option<u32>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cancel {
    /// Optional human-readable cancellation reason.
    pub reason: Option<String>,
    /// Optional 128-bit request id the cancel targets. V1 payloads encoded
    /// before this field existed carry no target byte; decoding them yields
    /// `None`, which the guest interprets as "cancel the single active
    /// request" (the V1 single-active contract). `Some(id)` is the explicit
    /// form used by `stream.stop`, letting the guest verify the request it is
    /// cancelling really is the one the caller asked to stop.
    pub target: Option<[u8; 16]>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fs {
    pub op: u8,
    pub path: String,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Health {
    pub healthy: bool,
    pub message: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: u32,
    pub message: String,
}

fn err(s: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn put(out: &mut Vec<u8>, b: &[u8]) -> io::Result<()> {
    let n = u32::try_from(b.len()).map_err(|_| err("field too large"))?;
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(b);
    Ok(())
}
fn get<'a>(b: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    if b.len() < 4 {
        return Err(err("truncated payload"));
    };
    let n = u32::from_be_bytes(b[..4].try_into().unwrap()) as usize;
    *b = &b[4..];
    if b.len() < n {
        return Err(err("truncated payload"));
    };
    let x = &b[..n];
    *b = &b[n..];
    Ok(x)
}
fn text(out: &mut Vec<u8>, s: &str) -> io::Result<()> {
    put(out, s.as_bytes())
}
fn gettext(b: &mut &[u8]) -> io::Result<String> {
    String::from_utf8(get(b)?.to_vec()).map_err(|_| err("invalid utf8"))
}
macro_rules! codec {
    ($t:ty,$enc:expr,$dec:expr) => {
        impl $t {
            pub fn encode(&self) -> io::Result<Vec<u8>> {
                $enc(self)
            }
            pub fn decode(mut b: &[u8]) -> io::Result<Self> {
                let v = $dec(&mut b)?;
                if !b.is_empty() {
                    return Err(err("trailing payload"));
                }
                Ok(v)
            }
        }
    };
}
codec!(
    Hello,
    |x: &Hello| {
        let mut o = Vec::new();
        text(&mut o, &x.client)?;
        o.push(u8::try_from(x.capabilities.len()).map_err(|_| err("too many capabilities"))?);
        for s in &x.capabilities {
            text(&mut o, s)?
        }
        Ok(o)
    },
    |b: &mut &[u8]| {
        let client = gettext(b)?;
        if b.is_empty() {
            return Err(err("truncated payload"));
        };
        let n = b[0] as usize;
        *b = &b[1..];
        let mut capabilities = Vec::new();
        for _ in 0..n {
            capabilities.push(gettext(b)?)
        }
        Ok(Hello {
            client,
            capabilities,
        })
    }
);
codec!(
    Execute,
    |x: &Execute| {
        let mut o = Vec::new();
        o.push(u8::try_from(x.argv.len()).map_err(|_| err("too many arguments"))?);
        for s in &x.argv {
            text(&mut o, s)?
        }
        match &x.cwd {
            Some(s) => {
                o.push(1);
                text(&mut o, s)?
            }
            None => o.push(0),
        }
        put(&mut o, &x.stdin)?;
        o.extend_from_slice(&x.timeout_ms.to_be_bytes());
        Ok(o)
    },
    |b: &mut &[u8]| {
        if b.is_empty() {
            return Err(err("truncated payload"));
        };
        let n = b[0] as usize;
        *b = &b[1..];
        let mut argv = Vec::new();
        for _ in 0..n {
            argv.push(gettext(b)?)
        }
        let cwd = if getflag(b)? { Some(gettext(b)?) } else { None };
        let stdin = get(b)?.to_vec();
        let timeout_ms = u32::from_be_bytes(getn(b, 4)?.try_into().unwrap());
        Ok(Execute {
            argv,
            cwd,
            stdin,
            timeout_ms,
        })
    }
);
fn getn<'a>(b: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if b.len() < n {
        return Err(err("truncated payload"));
    };
    let x = &b[..n];
    *b = &b[n..];
    Ok(x)
}
fn getflag(b: &mut &[u8]) -> io::Result<bool> {
    Ok(getn(b, 1)?[0] != 0)
}
codec!(
    Output,
    |x: &Output| {
        let mut o = vec![x.stream];
        put(&mut o, &x.data)?;
        Ok(o)
    },
    |b: &mut &[u8]| {
        let stream = getn(b, 1)?[0];
        Ok::<Output, io::Error>(Output {
            stream,
            data: get(b)?.to_vec(),
        })
    }
);
codec!(
    Exit,
    |x: &Exit| {
        let mut o = x.code.to_be_bytes().to_vec();
        o.push(x.signal.is_some() as u8);
        if let Some(v) = x.signal {
            o.extend_from_slice(&v.to_be_bytes())
        }
        Ok(o)
    },
    |b: &mut &[u8]| {
        let code = i32::from_be_bytes(getn(b, 4)?.try_into().unwrap());
        let signal = if getflag(b)? {
            Some(u32::from_be_bytes(getn(b, 4)?.try_into().unwrap()))
        } else {
            None
        };
        Ok::<Exit, io::Error>(Exit { code, signal })
    }
);
codec!(
    Cancel,
    |x: &Cancel| {
        let mut o = Vec::new();
        match &x.reason {
            Some(s) => {
                o.push(1);
                text(&mut o, s)?
            }
            None => o.push(0),
        }
        match &x.target {
            Some(id) => {
                o.push(1);
                o.extend_from_slice(id);
            }
            None => o.push(0),
        }
        Ok(o)
    },
    |b: &mut &[u8]| {
        let reason = if getflag(b)? { Some(gettext(b)?) } else { None };
        // Legacy V1 payloads (encoded before Cancel gained a target) stop
        // right after the reason flag; treat them as targeting the active
        // request (target = None) instead of failing on the missing byte.
        let target = if b.is_empty() {
            None
        } else if getflag(b)? {
            Some(getn(b, 16)?.try_into().unwrap())
        } else {
            None
        };
        Ok::<Cancel, io::Error>(Cancel { reason, target })
    }
);
codec!(
    Fs,
    |x: &Fs| {
        let mut o = vec![x.op];
        text(&mut o, &x.path)?;
        put(&mut o, &x.data)?;
        Ok(o)
    },
    |b: &mut &[u8]| Ok::<Fs, io::Error>(Fs {
        op: getn(b, 1)?[0],
        path: gettext(b)?,
        data: get(b)?.to_vec()
    })
);
codec!(
    Health,
    |x: &Health| {
        let mut o = vec![x.healthy as u8];
        match &x.message {
            Some(s) => {
                o.push(1);
                text(&mut o, s)?
            }
            None => o.push(0),
        }
        Ok(o)
    },
    |b: &mut &[u8]| Ok::<Health, io::Error>(Health {
        healthy: getflag(b)?,
        message: if getflag(b)? { Some(gettext(b)?) } else { None }
    })
);
codec!(
    Error,
    |x: &Error| {
        let mut o = x.code.to_be_bytes().to_vec();
        text(&mut o, &x.message)?;
        Ok(o)
    },
    |b: &mut &[u8]| Ok::<Error, io::Error>(Error {
        code: u32::from_be_bytes(getn(b, 4)?.try_into().unwrap()),
        message: gettext(b)?
    })
);

codec!(
    HelloAck,
    |x: &HelloAck| {
        let mut o = Vec::new();
        text(&mut o, &x.server)?;
        o.push(u8::try_from(x.capabilities.len()).map_err(|_| err("too many capabilities"))?);
        for s in &x.capabilities {
            text(&mut o, s)?;
        }
        Ok(o)
    },
    |b: &mut &[u8]| {
        let server = gettext(b)?;
        let n = getn(b, 1)?[0] as usize;
        let mut capabilities = Vec::with_capacity(n);
        for _ in 0..n {
            capabilities.push(gettext(b)?);
        }
        Ok::<HelloAck, io::Error>(HelloAck {
            server,
            capabilities,
        })
    }
);
