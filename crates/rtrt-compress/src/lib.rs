//! rtrt-compress — terse-mode rewriter for AI agent output.
//!
//! Rules-based string transforms for Output Optimizer terse mode.
//! Code blocks, inline code, URLs, and quoted error strings are preserved.
//!
//! Four levels:
//! - `lite`   — fillers (just/really/basically/…) + multi-space collapse only.
//! - `full`   — `lite` + pleasantries (sure/certainly/happy to/…) + hedging
//!   (I think/perhaps/maybe/…) + discourse markers (moreover/however/…) +
//!   articles (a/an/the).
//! - `ultra`  — `full` + arrows for causality, abbreviations, conjunction
//!   stripping, and phrase shortening.
//! - `extreme` — `ultra` + verbose qualifiers (very/extremely/…) +
//!   meta-phrases (it should be noted that, it is worth mentioning that, …)
//!   collapsed away. Readable but terse.

use once_cell::sync::Lazy;
use regex::Regex;
use rtrt_core::CompressionLevel;

pub mod ml;
#[cfg(feature = "onnx")]
pub mod ml_onnx;
pub mod secrets;

#[cfg(feature = "llm-compress")]
pub mod llm;
#[cfg(feature = "treesitter")]
pub mod treesitter;

#[cfg(feature = "llm-compress")]
pub use llm::{AsyncCompressor, LlmCompressor};
pub use ml::{CompressionTarget, HeuristicImportance, MlCompressor, TokenImportance};
pub use secrets::redact_secrets;
#[cfg(feature = "treesitter")]
pub use treesitter::{Language, SignatureExtractor};

#[derive(Debug, Clone, Copy)]
pub struct Compressor {
    pub level: CompressionLevel,
}

impl Compressor {
    pub fn new(level: CompressionLevel) -> Self {
        Self { level }
    }

    /// Compresses `input`. The pipeline:
    /// 1. Redact secret-shaped substrings (AWS / GitHub / OpenAI / Bearer / …).
    /// 2. Stash code blocks, inline code, URLs, and `"quoted strings"` into
    ///    placeholders so rules never touch them.
    /// 3. Apply the level-dependent ordered rule set.
    /// 4. Restore placeholders.
    pub fn compress(&self, input: &str) -> String {
        let redacted = redact_secrets(input);
        let (placeheld, slots) = stash_protected(&redacted);
        let mut out = placeheld;
        for (regex, replacement) in rules_for(self.level) {
            out = regex.replace_all(&out, *replacement).into_owned();
        }
        restore_protected(&out, &slots)
    }

    /// Compresses `input` and wraps the result in the chosen output format —
    /// inspired by chroma's per-collection encoder choices. The compressed
    /// payload is identical across formats; only the framing differs so a
    /// downstream agent can pick the encoding that fits its prompt budget.
    pub fn compress_to(&self, input: &str, format: OutputFormat) -> String {
        let body = self.compress(input);
        match format {
            OutputFormat::Plain => body,
            OutputFormat::Markdown => format!(
                "```text rtrt-level={}\n{}\n```\n",
                level_slug(self.level),
                body.trim_end_matches('\n')
            ),
            OutputFormat::Xml => {
                let escaped = xml_escape(&body);
                format!(
                    "<rtrt level=\"{}\"><![CDATA[{}]]></rtrt>",
                    level_slug(self.level),
                    escaped
                )
            }
            OutputFormat::Json => serde_json::json!({
                "level": level_slug(self.level),
                "content": body,
            })
            .to_string(),
        }
    }
}

/// Output encoding picked by the caller. `Plain` keeps the historical
/// behaviour (`compress()` is `compress_to(.., Plain)`). The structured
/// variants make it cheap for an agent to parse the compressed payload back
/// out of a tool-call response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Plain,
    Markdown,
    Xml,
    Json,
}

impl OutputFormat {
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "plain" | "text" | "txt" => Some(OutputFormat::Plain),
            "md" | "markdown" => Some(OutputFormat::Markdown),
            "xml" => Some(OutputFormat::Xml),
            "json" => Some(OutputFormat::Json),
            _ => None,
        }
    }
}

fn level_slug(level: CompressionLevel) -> &'static str {
    match level {
        CompressionLevel::Lite => "lite",
        CompressionLevel::Full => "full",
        CompressionLevel::Ultra => "ultra",
        CompressionLevel::Extreme => "extreme",
    }
}

fn xml_escape(input: &str) -> String {
    // CDATA can carry every character except the literal terminator. Splice
    // any `]]>` so we can't break out of the section.
    input.replace("]]>", "]]]]><![CDATA[>")
}

type Rule = (&'static Regex, &'static str);

// ---------- atomic rule regexes ----------

static FILLERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(just|really|basically|actually|simply|literally|honestly|frankly|truly|essentially|kind of|sort of)\s+",
    )
    .unwrap()
});

static PLEASANTRIES: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(sure|certainly|of course|happy to|let me|i'll|i can|i would|i'd be happy to)\b[,!\.]?\s*",
    )
    .unwrap()
});

static HEDGES: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(i (think|believe|suspect|guess|imagine|suppose)( that)?|in my opinion|to my mind|if you ask me|perhaps|maybe|probably|possibly|likely|it (seems|appears) (that|to be)|i('m| am) (pretty |fairly |reasonably )?(sure|confident)( that)?|if i('m| am) not mistaken|if i recall correctly)[,!\.]?\s*",
    )
    .unwrap()
});

static DISCOURSE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(moreover|furthermore|additionally|besides|however|nevertheless|nonetheless|as (you|we) can see|as a matter of fact|needless to say|it('s| is) worth (noting|mentioning) that|of course|naturally|obviously|clearly)\b[,!\.]?\s*",
    )
    .unwrap()
});

static ARTICLES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(a|an|the)\s+").unwrap());

static MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t]{2,}").unwrap());

static MULTI_NEWLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());

// Verbose qualifiers — leftover adjectives/adverbs after FILLERS that still
// add no signal in agent output.
static QUALIFIERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(very|extremely|quite|rather|fairly|somewhat|extremely|highly)\s+").unwrap()
});

// Meta-phrases that announce intent without adding info. Always-on at full+ to
// match Output Optimizer's "drop hedging" rule.
static META_PHRASES: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(it is important to (note|remember|understand|mention) that |it should be noted that |it is worth (mentioning|noting|pointing out) that |as we (mentioned|noted|discussed) (earlier|above|previously),? )",
    )
    .unwrap()
});

// ---------- phrase-shortening regexes (paired with replacement strings) ----------

static PHRASE_DUE_TO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bdue to the fact that\b").unwrap());
static PHRASE_IN_ORDER_TO: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bin order to\b").unwrap());
static PHRASE_AT_THIS_POINT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bat this point in time\b").unwrap());
static PHRASE_FOR_THE_PURPOSE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bfor the purpose of\b").unwrap());
static PHRASE_IN_THE_EVENT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bin the event that\b").unwrap());
static PHRASE_WITH_THE_EXCEPTION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bwith the exception of\b").unwrap());
static PHRASE_A_NUMBER_OF: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\ba number of\b").unwrap());
static PHRASE_THE_MAJORITY_OF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bthe majority of\b").unwrap());
static PHRASE_IN_SPITE_OF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bin spite of( the fact that)?\b").unwrap());
static PHRASE_ON_THE_BASIS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bon the basis of\b").unwrap());
static PHRASE_FOR_INSTANCE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bfor (instance|example)\b").unwrap());
static CAUSAL_CONNECTORS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\s*,?\s+\b(because(?:\s+of)?|therefore|thus|leads\s+to)\b\s+").unwrap()
});
static CAUSAL_SO: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)(^|[.;,])\s*\bso\b\s+").unwrap());
static COORDINATING_CONJUNCTIONS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\s+\b(and|but|yet)\b\s+").unwrap());

static ABBR_AUTHENTICATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bauthentication\b").unwrap());
static ABBR_AUTHORIZATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bauthorization\b").unwrap());
static ABBR_CONFIGURATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bconfiguration\b").unwrap());
static ABBR_IMPLEMENTATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bimplementation\b").unwrap());
static ABBR_REPOSITORY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\brepository\b").unwrap());
static ABBR_DIRECTORY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bdirectory\b").unwrap());
static ABBR_FUNCTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bfunction\b").unwrap());
static ABBR_REQUEST: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\brequest\b").unwrap());
static ABBR_RESPONSE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bresponse\b").unwrap());
static ABBR_DATABASE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bdatabase\b").unwrap());
static ABBR_OBJECT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bobject\b").unwrap());
static ABBR_REFERENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\breference\b").unwrap());
static ABBR_CONNECTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bconnection\b").unwrap());
static ABBR_COMMAND: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bcommand\b").unwrap());
static ABBR_APPLICATION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bapplication\b").unwrap());
static ABBR_DEPENDENCY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bdependency\b").unwrap());
static ABBR_DEPENDENCIES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bdependencies\b").unwrap());
static ABBR_PARAMETER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bparameter\b").unwrap());
static ABBR_PARAMETERS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bparameters\b").unwrap());
static ABBR_ARGUMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bargument\b").unwrap());
static ABBR_ARGUMENTS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\barguments\b").unwrap());
static ABBR_ENVIRONMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\benvironment\b").unwrap());
static ABBR_VARIABLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bvariable\b").unwrap());
static ABBR_VARIABLES: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bvariables\b").unwrap());
static ABBR_MESSAGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bmessage\b").unwrap());
static ABBR_ERROR: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\berror\b").unwrap());
static ABBR_DOCUMENTATION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\bdocumentation\b").unwrap());
static ABBR_DOCUMENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bdocument\b").unwrap());
static ABBR_PACKAGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bpackage\b").unwrap());
static ABBR_SOURCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bsource\b").unwrap());
static ABBR_DESTINATION: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bdestination\b").unwrap());
static ABBR_TEMPORARY: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\btemporary\b").unwrap());

static LITE_RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    vec![
        (&*FILLERS, ""),
        (&*MULTI_SPACE, " "),
        (&*MULTI_NEWLINE, "\n\n"),
    ]
});

static FULL_RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    vec![
        (&*FILLERS, ""),
        (&*PLEASANTRIES, ""),
        (&*HEDGES, ""),
        (&*DISCOURSE, ""),
        (&*META_PHRASES, ""),
        (&*ARTICLES, ""),
        (&*MULTI_SPACE, " "),
        (&*MULTI_NEWLINE, "\n\n"),
    ]
});

static ULTRA_RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    vec![
        (&*FILLERS, ""),
        (&*PLEASANTRIES, ""),
        (&*HEDGES, ""),
        (&*DISCOURSE, ""),
        (&*META_PHRASES, ""),
        (&*ARTICLES, ""),
        (&*CAUSAL_CONNECTORS, " -> "),
        (&*CAUSAL_SO, "$1 -> "),
        (&*COORDINATING_CONJUNCTIONS, " "),
        (&*PHRASE_DUE_TO, "because"),
        (&*PHRASE_IN_ORDER_TO, "to"),
        (&*PHRASE_AT_THIS_POINT, "now"),
        (&*PHRASE_FOR_THE_PURPOSE, "for"),
        (&*PHRASE_IN_THE_EVENT, "if"),
        (&*PHRASE_WITH_THE_EXCEPTION, "except"),
        (&*PHRASE_A_NUMBER_OF, "several"),
        (&*PHRASE_THE_MAJORITY_OF, "most"),
        (&*PHRASE_IN_SPITE_OF, "despite"),
        (&*PHRASE_ON_THE_BASIS, "based on"),
        (&*PHRASE_FOR_INSTANCE, "e.g."),
        (&*ABBR_AUTHENTICATION, "auth"),
        (&*ABBR_AUTHORIZATION, "authz"),
        (&*ABBR_CONFIGURATION, "config"),
        (&*ABBR_IMPLEMENTATION, "impl"),
        (&*ABBR_REPOSITORY, "repo"),
        (&*ABBR_DIRECTORY, "dir"),
        (&*ABBR_FUNCTION, "fn"),
        (&*ABBR_REQUEST, "req"),
        (&*ABBR_RESPONSE, "res"),
        (&*ABBR_DATABASE, "DB"),
        (&*ABBR_OBJECT, "obj"),
        (&*ABBR_REFERENCE, "ref"),
        (&*ABBR_CONNECTION, "conn"),
        (&*ABBR_COMMAND, "cmd"),
        (&*ABBR_APPLICATION, "app"),
        (&*ABBR_DEPENDENCIES, "deps"),
        (&*ABBR_DEPENDENCY, "dep"),
        (&*ABBR_PARAMETERS, "params"),
        (&*ABBR_PARAMETER, "param"),
        (&*ABBR_ARGUMENTS, "args"),
        (&*ABBR_ARGUMENT, "arg"),
        (&*ABBR_ENVIRONMENT, "env"),
        (&*ABBR_VARIABLES, "vars"),
        (&*ABBR_VARIABLE, "var"),
        (&*ABBR_MESSAGE, "msg"),
        (&*ABBR_ERROR, "err"),
        (&*ABBR_DOCUMENTATION, "docs"),
        (&*ABBR_DOCUMENT, "doc"),
        (&*ABBR_PACKAGE, "pkg"),
        (&*ABBR_SOURCE, "src"),
        (&*ABBR_DESTINATION, "dst"),
        (&*ABBR_TEMPORARY, "temp"),
        (&*MULTI_SPACE, " "),
        (&*MULTI_NEWLINE, "\n\n"),
    ]
});

static EXTREME_RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    vec![
        (&*FILLERS, ""),
        (&*PLEASANTRIES, ""),
        (&*HEDGES, ""),
        (&*DISCOURSE, ""),
        (&*META_PHRASES, ""),
        (&*QUALIFIERS, ""),
        (&*ARTICLES, ""),
        (&*CAUSAL_CONNECTORS, " -> "),
        (&*CAUSAL_SO, "$1 -> "),
        (&*COORDINATING_CONJUNCTIONS, " "),
        (&*PHRASE_DUE_TO, "because"),
        (&*PHRASE_IN_ORDER_TO, "to"),
        (&*PHRASE_AT_THIS_POINT, "now"),
        (&*PHRASE_FOR_THE_PURPOSE, "for"),
        (&*PHRASE_IN_THE_EVENT, "if"),
        (&*PHRASE_WITH_THE_EXCEPTION, "except"),
        (&*PHRASE_A_NUMBER_OF, "several"),
        (&*PHRASE_THE_MAJORITY_OF, "most"),
        (&*PHRASE_IN_SPITE_OF, "despite"),
        (&*PHRASE_ON_THE_BASIS, "based on"),
        (&*PHRASE_FOR_INSTANCE, "e.g."),
        (&*ABBR_AUTHENTICATION, "auth"),
        (&*ABBR_AUTHORIZATION, "authz"),
        (&*ABBR_CONFIGURATION, "config"),
        (&*ABBR_IMPLEMENTATION, "impl"),
        (&*ABBR_REPOSITORY, "repo"),
        (&*ABBR_DIRECTORY, "dir"),
        (&*ABBR_FUNCTION, "fn"),
        (&*ABBR_REQUEST, "req"),
        (&*ABBR_RESPONSE, "res"),
        (&*ABBR_DATABASE, "DB"),
        (&*ABBR_OBJECT, "obj"),
        (&*ABBR_REFERENCE, "ref"),
        (&*ABBR_CONNECTION, "conn"),
        (&*ABBR_COMMAND, "cmd"),
        (&*ABBR_APPLICATION, "app"),
        (&*ABBR_DEPENDENCIES, "deps"),
        (&*ABBR_DEPENDENCY, "dep"),
        (&*ABBR_PARAMETERS, "params"),
        (&*ABBR_PARAMETER, "param"),
        (&*ABBR_ARGUMENTS, "args"),
        (&*ABBR_ARGUMENT, "arg"),
        (&*ABBR_ENVIRONMENT, "env"),
        (&*ABBR_VARIABLES, "vars"),
        (&*ABBR_VARIABLE, "var"),
        (&*ABBR_MESSAGE, "msg"),
        (&*ABBR_ERROR, "err"),
        (&*ABBR_DOCUMENTATION, "docs"),
        (&*ABBR_DOCUMENT, "doc"),
        (&*ABBR_PACKAGE, "pkg"),
        (&*ABBR_SOURCE, "src"),
        (&*ABBR_DESTINATION, "dst"),
        (&*ABBR_TEMPORARY, "temp"),
        (&*MULTI_SPACE, " "),
        (&*MULTI_NEWLINE, "\n\n"),
    ]
});

fn rules_for(level: CompressionLevel) -> &'static [Rule] {
    match level {
        CompressionLevel::Lite => LITE_RULES.as_slice(),
        CompressionLevel::Full => FULL_RULES.as_slice(),
        CompressionLevel::Ultra => ULTRA_RULES.as_slice(),
        CompressionLevel::Extreme => EXTREME_RULES.as_slice(),
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
    fn full_drops_articles_outside_code() {
        let c = Compressor::new(CompressionLevel::Full);
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

    #[test]
    fn full_drops_hedges() {
        let c = Compressor::new(CompressionLevel::Full);
        let out = c.compress("I think the parser has a bug, perhaps in the lexer.");
        assert!(!out.to_lowercase().contains("i think"), "{out}");
        assert!(!out.to_lowercase().contains("perhaps"), "{out}");
        assert!(!out.to_lowercase().contains(" a "), "{out}");
        assert!(!out.to_lowercase().contains(" the "), "{out}");
    }

    #[test]
    fn ultra_rewrites_phrases_arrows_and_abbreviations() {
        let c = Compressor::new(CompressionLevel::Ultra);
        let out = c.compress("The implementation is a function because of the configuration.");
        assert!(out.contains("impl"), "{out}");
        assert!(out.contains("fn"), "{out}");
        assert!(out.contains("config"), "{out}");
        assert!(out.contains(" -> "), "{out}");
        assert!(!out.to_lowercase().contains("because"), "{out}");
    }

    #[test]
    fn ultra_rewrites_long_phrases() {
        let c = Compressor::new(CompressionLevel::Ultra);
        let out = c.compress("Due to the fact that we forgot to escape the input, in order to fix the bug, we need to add quotes.");
        assert!(
            !out.to_lowercase().contains("due to the fact that"),
            "{out}"
        );
        assert!(!out.to_lowercase().contains("in order to"), "{out}");
    }

    #[test]
    fn code_spans_survive_all_levels() {
        let input = "The implementation is `the function because of configuration`.\n```rust\nlet the_value = \"because\";\n```";
        for level in [
            CompressionLevel::Lite,
            CompressionLevel::Full,
            CompressionLevel::Ultra,
            CompressionLevel::Extreme,
        ] {
            let out = Compressor::new(level).compress(input);
            assert!(
                out.contains("`the function because of configuration`"),
                "{level:?}: {out}"
            );
            assert!(
                out.contains("```rust\nlet the_value = \"because\";\n```"),
                "{level:?}: {out}"
            );
        }
    }

    #[test]
    fn extreme_drops_qualifiers() {
        let c = Compressor::new(CompressionLevel::Extreme);
        let out = c.compress("This is a very extremely complex problem, but it is rather elegant.");
        assert!(!out.to_lowercase().contains("very"), "{out}");
        assert!(!out.to_lowercase().contains("extremely"), "{out}");
        assert!(!out.to_lowercase().contains("rather"), "{out}");
    }

    #[test]
    fn redacts_aws_key() {
        let c = Compressor::new(CompressionLevel::Full);
        let out = c.compress("aws key AKIAIOSFODNN7EXAMPLE goes here");
        assert!(out.contains("<REDACTED:"), "{out}");
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "{out}");
    }

    #[test]
    fn output_format_plain_matches_compress() {
        let c = Compressor::new(CompressionLevel::Full);
        let input = "I think the parser has a bug, perhaps in the lexer.";
        assert_eq!(c.compress(input), c.compress_to(input, OutputFormat::Plain));
    }

    #[test]
    fn output_format_markdown_wraps_in_fence() {
        let c = Compressor::new(CompressionLevel::Lite);
        let out = c.compress_to("the bug is really bad", OutputFormat::Markdown);
        assert!(out.starts_with("```text rtrt-level=lite\n"), "{out}");
        assert!(out.trim_end().ends_with("```"), "{out}");
        assert!(!out.contains("really"), "{out}");
    }

    #[test]
    fn output_format_xml_wraps_in_cdata() {
        let c = Compressor::new(CompressionLevel::Ultra);
        let out = c.compress_to("the bug is in the parser", OutputFormat::Xml);
        assert!(out.starts_with("<rtrt level=\"ultra\"><![CDATA["), "{out}");
        assert!(out.ends_with("]]></rtrt>"), "{out}");
    }

    #[test]
    fn output_format_json_is_parseable() {
        let c = Compressor::new(CompressionLevel::Full);
        let out = c.compress_to("really bad bug", OutputFormat::Json);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["level"], "full");
        assert!(v["content"].as_str().unwrap().contains("bug"));
    }

    #[test]
    fn output_format_parse_accepts_aliases() {
        assert_eq!(OutputFormat::parse("md"), Some(OutputFormat::Markdown));
        assert_eq!(
            OutputFormat::parse("MARKDOWN"),
            Some(OutputFormat::Markdown)
        );
        assert_eq!(OutputFormat::parse("xml"), Some(OutputFormat::Xml));
        assert_eq!(OutputFormat::parse("text"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::parse("nope"), None);
    }
}
