use std::hint::black_box;
use std::time::{Duration, Instant};

use rfb_runtime::codec::{FrameCodec, MessageType};

mod mem;

const ITERATIONS: usize = 1_000;

fn benchmark(codec: &FrameCodec, payload: &str) -> (Duration, Duration, usize) {
    let mut encoded_size = 0;
    let encode_start = Instant::now();
    for sequence in 0..ITERATIONS {
        let frame = codec
            .encode(MessageType::Event, sequence as u64, &payload)
            .expect("benchmark payload must fit codec limit");
        encoded_size = frame.len();
        black_box(frame);
    }
    let encode_elapsed = encode_start.elapsed();

    let frame = codec
        .encode(MessageType::Event, 0, &payload)
        .expect("benchmark payload must fit codec limit");
    let decode_start = Instant::now();
    for _ in 0..ITERATIONS {
        let (_, value): (_, String) = codec.decode(black_box(&frame)).expect("frame decodes");
        black_box(value);
    }
    let decode_elapsed = decode_start.elapsed();
    (encode_elapsed, decode_elapsed, encoded_size)
}

fn main() {
    let codec = FrameCodec::default();
    let baseline_rss = mem::current_rss_bytes();
    let baseline_peak = mem::peak_rss_bytes();
    println!("rfb-runtime codec benchmark ({ITERATIONS} iterations per case)");
    println!(
        "baseline current_rss={} peak_rss={}",
        baseline_rss
            .map(|b| format!("{b} bytes"))
            .unwrap_or_else(|| "unavailable".into()),
        baseline_peak
            .map(|b| format!("{b} bytes"))
            .unwrap_or_else(|| "unavailable".into()),
    );
    for size in [64usize, 1_024, 16 * 1_024, 1_024 * 1_024] {
        let payload = "x".repeat(size);
        let (encode, decode, encoded_size) = benchmark(&codec, &payload);
        let encode_rate = ITERATIONS as f64 / encode.as_secs_f64();
        let decode_rate = ITERATIONS as f64 / decode.as_secs_f64();
        let current = mem::current_rss_bytes()
            .zip(baseline_rss)
            .map(|(now, base)| now.saturating_sub(base))
            .map(|delta| format!("{delta} bytes"))
            .unwrap_or_else(|| "unavailable".into());
        let peak = mem::peak_rss_bytes()
            .map(|peak| format!("{peak} bytes"))
            .unwrap_or_else(|| "unavailable".into());
        println!(
            "payload={size}B encoded={encoded_size}B encode={encode:?} ({encode_rate:.0} ops/s) decode={decode:?} ({decode_rate:.0} ops/s) rss_delta_since_baseline={current} vmhwm={peak}"
        );
    }
}
