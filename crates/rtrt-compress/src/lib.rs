//! rtrt-compress — terse-mode rewriter for AI agent output.
//!
//! Rules-based string transforms inspired by `JuliusBrussee/caveman`.
//! Code blocks, inline code, URLs, and quoted error strings are preserved.

use once_cell::sync::Lazy;
use regex::Regex;
use rtrt_core::CompressionLevel;

#[derive(Debug, Clone, Copy)]
pub struct Compressor {
    pub level: CompressionLevel,
}

impl Compressor {
    pub fn new(level: CompressionLevel) -> Self {
        Self { level }
    }

    pub fn compress(&self, input: &str) -> String {
        let (placeheld, slots) = stash_protected(input);
        let mut out = placeheld;
        for (regex, replacement) in rules_for(self.level) {
            out = regex.replace_all(&out, *replacement).into_owned();
        }
        restore_protected(&out, &slots)
    }
}

type Rule = (&'static Regex, &'static str);

static FILLERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(just|really|basically|actually|simply|very|quite|literally)\s+").unwrap()
});

static PLEASANTRIES: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(sure|certainly|of course|happy to|let me|i'll|i can|i would)\b[,!\.]?\s*",
    )
    .unwrap()
});

static ARTICLES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(a|an|the)\s+").unwrap());

static MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t]{2,}").unwrap());

static LITE_RULES: Lazy<Vec<Rule>> =
    Lazy::new(|| vec![(&*FILLERS, ""), (&*MULTI_SPACE, " ")]);

static FULL_RULES: Lazy<Vec<Rule>> =
    Lazy::new(|| vec![(&*FILLERS, ""), (&*PLEASANTRIES, ""), (&*MULTI_SPACE, " ")]);

static ULTRA_RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    vec![(&*FILLERS, ""), (&*PLEASANTRIES, ""), (&*ARTICLES, ""), (&*MULTI_SPACE, " ")]
});

fn rules_for(level: CompressionLevel) -> &'static [Rule] {
    match level {
        CompressionLevel::Lite => LITE_RULES.as_slice(),
        CompressionLevel::Full => FULL_RULES.as_slice(),
        CompressionLevel::Ultra => ULTRA_RULES.as_slice(),
    }
}

static PROTECT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?s)```.*?```|`[^`]*`|https?://\S+|"[^"]*""#).unwrap());

fn stash_protected(input: &str) -> (String, Vec<String>) {
    let mut slots: Vec<String> = Vec::new();
    let out = PROTECT
        .replace_all(input, |caps: &regex::Captures<'_>| {
            let token = format!("\u{0001}RTRT_PROTECT_{}\u{0002}", slots.len());
            slots.push(caps[0].to_string());
            token
        })
        .into_owned();
    (out, slots)
}

fn restore_protected(input: &str, slots: &[String]) -> String {
    let mut out = input.to_string();
    for (i, original) in slots.iter().enumerate() {
        let needle = format!("\u{0001}RTRT_PROTECT_{i}\u{0002}");
        out = out.replace(&needle, original);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_code_block() {
        let c = Compressor::new(CompressionLevel::Ultra);
        let input = "the value is `the answer` always";
        let out = c.compress(input);
        assert!(out.contains("`the answer`"), "{out}");
    }

    #[test]
    fn ultra_drops_articles_outside_code() {
        let c = Compressor::new(CompressionLevel::Ultra);
        let out = c.compress("the bug is in the parser");
        assert!(!out.contains("the bug"), "{out}");
    }

    #[test]
    fn lite_keeps_articles() {
        let c = Compressor::new(CompressionLevel::Lite);
        let out = c.compress("the bug is really bad");
        assert!(out.contains("the bug"), "{out}");
        assert!(!out.contains("really"), "{out}");
    }
}
