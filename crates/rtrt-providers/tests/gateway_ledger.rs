//! Gateway → provider-usage ledger recording.
//!
//! Every dispatched request on a recording gateway must land in the TSV
//! ledger with the documented conventions: real API usage recorded exactly
//! (`est = 0`), chars/4 estimates when no usage is available (`est = 1`),
//! failures as `ok = 0`. Non-recording gateways must stay ledger-silent.
//!
//! One test function on purpose: the ledger path is selected via the
//! process-global `RTRT_PROVIDER_USAGE_PATH` env var, so splitting into
//! parallel tests would race on it.

use async_trait::async_trait;
use rtrt_providers::{ChatMessage, ChatRequest, ChatResponse, Gateway, Provider, Role, Usage};

struct Scripted {
    name: &'static str,
    tokens: Option<(u64, u64)>,
    fail: bool,
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        self.name
    }
    fn supported_models(&self) -> &[&'static str] {
        &[]
    }
    async fn chat(&self, req: ChatRequest) -> rtrt_core::Result<ChatResponse> {
        if self.fail {
            return Err(rtrt_core::Error::Provider("simulated 503".into()));
        }
        let (input, output) = self.tokens.unwrap_or_default();
        Ok(ChatResponse {
            provider: self.name.to_string(),
            model: req.model,
            content: "12345678".to_string(), // 8 chars → 2 estimated tokens
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                ..Default::default()
            },
        })
    }
}

fn req(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![ChatMessage {
            role: Role::User,
            // 4 chars → exactly 1 estimated token at chars/4.
            content: "abcd".into(),
        }],
        max_tokens: None,
        temperature: None,
    }
}

/// Parse the raw TSV rows: (target, model, input, output, est, ok).
fn ledger_rows(path: &std::path::Path) -> Vec<(String, String, u64, u64, bool, bool)> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 7, "ledger row must have 7 fields: {line}");
            (
                f[1].to_string(),
                f[2].to_string(),
                f[3].parse().unwrap(),
                f[4].parse().unwrap(),
                f[5] != "0",
                f[6] != "0",
            )
        })
        .collect()
}

#[tokio::test]
async fn recording_gateway_writes_ledger_rows_per_convention() {
    let ledger = std::env::temp_dir().join(format!(
        "rtrt-gateway-ledger-{}-{}.tsv",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    // SAFETY: single-threaded at this point (one test fn in this binary), and
    // the var is only read by this crate's ledger-path resolution.
    unsafe { std::env::set_var("RTRT_PROVIDER_USAGE_PATH", &ledger) };

    let gw = Gateway::new()
        .with_usage_recording(true)
        .register(
            "real-usage",
            Box::new(Scripted {
                name: "real-usage",
                tokens: Some((11, 22)),
                fail: false,
            }),
            ["real-"],
        )
        .register(
            "no-usage",
            Box::new(Scripted {
                name: "no-usage",
                tokens: None,
                fail: false,
            }),
            ["nousage-"],
        )
        .register(
            "flaky",
            Box::new(Scripted {
                name: "flaky",
                tokens: None,
                fail: true,
            }),
            ["flaky-"],
        );

    // 1. Real API usage → exact counts, est = 0, ok = 1.
    gw.chat(req("real-model")).await.expect("real-usage chat");
    // 2. Success without a usage block → chars/4 estimate, est = 1, ok = 1.
    gw.chat(req("nousage-model")).await.expect("no-usage chat");
    // 3. Failure → estimated prompt tokens, 0 output, est = 1, ok = 0.
    gw.chat(req("flaky-model")).await.expect_err("flaky chat");

    let rows = ledger_rows(&ledger);
    assert_eq!(rows.len(), 3, "one ledger row per dispatched request");
    assert_eq!(
        rows[0],
        (
            "real-usage".into(),
            "real-model".into(),
            11,
            22,
            false,
            true
        )
    );
    // Prompt "abcd" → 1 token; content "12345678" → 2 tokens.
    assert_eq!(
        rows[1],
        ("no-usage".into(), "nousage-model".into(), 1, 2, true, true)
    );
    assert_eq!(
        rows[2],
        ("flaky".into(), "flaky-model".into(), 1, 0, true, false)
    );

    // 4. A non-recording gateway (the `Gateway::new` default) stays silent.
    let silent = Gateway::new().register(
        "real-usage",
        Box::new(Scripted {
            name: "real-usage",
            tokens: Some((1, 1)),
            fail: false,
        }),
        ["real-"],
    );
    silent.chat(req("real-model")).await.expect("silent chat");
    assert_eq!(
        ledger_rows(&ledger).len(),
        3,
        "non-recording gateway must not append"
    );

    let _ = std::fs::remove_file(&ledger);
}
