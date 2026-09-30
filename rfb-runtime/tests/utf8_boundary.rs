//! `Utf8ChunkDecoder` chunk-boundary contract (integration tests; moved
//! out of src/ per the check-tests-folder boundary).
mod tests {
    use rfb_runtime::utf8_boundary::{Utf8ChunkDecoder, MAX_SEQUENCE};

    fn feed(decoder: &mut Utf8ChunkDecoder, chunks: &[&[u8]]) -> String {
        chunks
            .iter()
            .map(|chunk| decoder.decode(chunk))
            .collect::<String>()
            + &decoder.flush()
    }

    #[test]
    fn multibyte_split_across_chunks_is_not_corrupted() {
        // "你" = E4 BD A0, "😀" = F0 9F 98 80 — split every which way.
        for cut in 1.."你😀".len() {
            let whole = "你😀".as_bytes();
            let mut decoder = Utf8ChunkDecoder::new();
            assert_eq!(feed(&mut decoder, &[&whole[..cut], &whole[cut..]]), "你😀");
        }
    }

    #[test]
    fn ascii_stream_is_unchanged() {
        let mut decoder = Utf8ChunkDecoder::new();
        assert_eq!(
            feed(&mut decoder, &[b"hel", b"lo ", b"world"]),
            "hello world"
        );
    }

    #[test]
    fn invalid_bytes_still_degrade_to_lossy() {
        let mut decoder = Utf8ChunkDecoder::new();
        // Lone continuation byte: cannot start a sequence, must not buffer.
        assert_eq!(feed(&mut decoder, &[&[0x80], &[0x41]]), "\u{FFFD}A");
        // Overlong lead byte (0xC0/0xC1): rejected, not withheld.
        let mut decoder = Utf8ChunkDecoder::new();
        assert_eq!(feed(&mut decoder, &[&[0xC1, 0x80]]), "\u{FFFD}\u{FFFD}");
    }

    #[test]
    fn truncated_sequence_at_eof_flushes_lossily() {
        let mut decoder = Utf8ChunkDecoder::new();
        assert_eq!(decoder.decode(&[0xE4, 0xBD]), "");
        assert_eq!(decoder.flush(), "\u{FFFD}");
    }

    #[test]
    fn withheld_tail_never_exceeds_three_bytes() {
        // A flood of lone continuation bytes must not accumulate.
        let mut decoder = Utf8ChunkDecoder::new();
        for _ in 0..1000 {
            decoder.decode(&[0x80]);
        }
        assert!(decoder.pending.len() < MAX_SEQUENCE);
    }
}
