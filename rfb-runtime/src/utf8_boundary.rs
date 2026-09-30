//! Incremental UTF-8 chunk decoding for byte-oriented output pipes.
//!
//! Output pipes are read in fixed-size chunks (`READ_CHUNK`, 32 KiB on the ZBRT
//! workspace executor; 8 KiB on the forkd agent stream). A multi-byte UTF-8
//! sequence can straddle a chunk boundary, and a per-chunk
//! `String::from_utf8_lossy` would turn the split halves into two U+FFFD
//! replacements — silent output corruption for any non-ASCII output.
//! [`Utf8ChunkDecoder`] buffers a trailing (possibly) partial sequence and
//! joins it with the next chunk, so the decoded text stream is byte-identical
//! to `String::from_utf8_lossy` over the *whole* stream. Genuinely invalid
//! bytes still degrade to lossy replacements instead of stalling the stream.

/// Maximum bytes a single UTF-8 sequence can occupy.
#[doc(hidden)]
pub const MAX_SEQUENCE: usize = 4;

/// Boundary-aware incremental decoder for one output stream.
#[derive(Debug, Default)]
pub struct Utf8ChunkDecoder {
    /// Withheld tail of the previous chunk: a (possibly) incomplete multi-byte
    /// sequence. Always shorter than [`MAX_SEQUENCE`].
    pub pending: Vec<u8>,
}

impl Utf8ChunkDecoder {
    /// A fresh decoder for one stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one chunk, holding back an incomplete trailing sequence so it
    /// can join the next chunk. The concatenation of all returned strings
    /// (plus [`Self::flush`]) is `from_utf8_lossy` over the whole stream.
    pub fn decode(&mut self, chunk: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(chunk);
        let split = utf8_safe_split(&bytes);
        self.pending = bytes[split..].to_vec();
        String::from_utf8_lossy(&bytes[..split]).into_owned()
    }

    /// End of stream: emit any withheld bytes (lossily — a truncated sequence
    /// at true EOF can never complete).
    pub fn flush(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }
}

/// The number of leading bytes of `bytes` that end on a character boundary
/// *and* leave only a plausibly-completable partial sequence in the tail. A
/// tail that is invalid UTF-8 in progress (a continuation run with no lead, or
/// a lead byte claiming more than 4 bytes) is treated as complete so the lossy
/// conversion handles it instead of buffering forever.
fn utf8_safe_split(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    // Walk back at most MAX_SEQUENCE-1 bytes: a complete trailing sequence is
    // at most 4 bytes, so a lead byte further back cannot own the tail.
    let start = bytes.len().saturating_sub(MAX_SEQUENCE - 1);
    for index in (start..bytes.len()).rev() {
        let byte = bytes[index];
        if byte & 0xC0 != 0x80 {
            // Lead byte (or ASCII) found. Its expected sequence length:
            let expected = match byte {
                0x00..=0x7F => 1,
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                // 0x80..=0xC1 / 0xF5..=0xFF can never start valid UTF-8:
                // treat the tail as complete and let lossy replace it.
                _ => return bytes.len(),
            };
            let have = bytes.len() - index;
            if have < expected {
                // Truncated (possibly) sequence: withhold it.
                return index;
            }
            // The full sequence is present; whether it is valid or not, lossy
            // on the whole range gives the same result as any split before it.
            return bytes.len();
        }
    }
    // Only continuation bytes in the inspected window (or none at all): the
    // tail is not a valid sequence start, so emit everything.
    bytes.len()
}
