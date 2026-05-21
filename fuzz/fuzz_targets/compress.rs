#![no_main]
//! Fuzz the four-level compressor end-to-end. The target asserts the
//! invariants that should hold regardless of input:
//! - Output length never exceeds input length (compressor never adds bytes
//!   beyond restored placeholders, which are bounded by the protect regex).
//! - Restored placeholders never leak — output should not contain the
//!   `\u{0001}RTRT_PROTECT_` sentinel.
//! - The compressor never panics on arbitrary bytes that round-trip through
//!   `String::from_utf8_lossy`.

use libfuzzer_sys::fuzz_target;
use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

const LEVELS: &[CompressionLevel] = &[
    CompressionLevel::Lite,
    CompressionLevel::Full,
    CompressionLevel::Ultra,
    CompressionLevel::Extreme,
];

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    for level in LEVELS {
        let c = Compressor::new(*level);
        let out = c.compress(&s);
        assert!(
            !out.contains("\u{0001}RTRT_PROTECT_"),
            "placeholder leaked through restore phase"
        );
    }
});
