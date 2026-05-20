//! Tiny example used by `scripts/bench.sh` to compute compressed byte counts
//! for a named fixture at a named compression level. Prints the post-compression
//! byte count to stdout; nothing else, so the shell wrapper can read it.

use std::process::ExitCode;

use rtrt_compress::Compressor;
use rtrt_core::CompressionLevel;

const FIXTURES: &[(&str, &str)] = &[
    ("short", include_str!("../benches/fixtures/short.md")),
    ("code", include_str!("../benches/fixtures/code.md")),
    ("mixed", include_str!("../benches/fixtures/mixed.md")),
    ("long", include_str!("../benches/fixtures/long.md")),
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut fixture: Option<String> = None;
    let mut level: Option<CompressionLevel> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => fixture = args.next(),
            "--level" => {
                level = match args.next().as_deref() {
                    Some("lite") => Some(CompressionLevel::Lite),
                    Some("full") => Some(CompressionLevel::Full),
                    Some("ultra") => Some(CompressionLevel::Ultra),
                    other => {
                        eprintln!("unknown level: {other:?}");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("unknown arg: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(name) = fixture else {
        eprintln!("--fixture required");
        return ExitCode::from(2);
    };
    let Some(level) = level else {
        eprintln!("--level required");
        return ExitCode::from(2);
    };
    let Some((_, body)) = FIXTURES.iter().find(|(n, _)| *n == name) else {
        eprintln!("unknown fixture: {name}");
        return ExitCode::from(2);
    };
    let compressed = Compressor::new(level).compress(body);
    print!("{}", compressed.len());
    ExitCode::SUCCESS
}
