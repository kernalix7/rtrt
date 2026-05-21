#![no_main]
//! Fuzz the secret redactor. Invariants:
//! - Output never contains the obvious tell-tales of any known secret shape
//!   that the redactor was meant to mask. We check this by re-running the
//!   redactor against the output — the second pass must be a no-op when the
//!   first pass produced no `<REDACTED:...>` markers, and idempotent (output
//!   equals input on the second pass) when it did.

use libfuzzer_sys::fuzz_target;
use rtrt_compress::redact_secrets;

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let pass1 = redact_secrets(&s);
    let pass2 = redact_secrets(&pass1);
    // Idempotence: re-running on a redacted string must not invent more changes.
    assert_eq!(pass1, pass2, "redact_secrets is not idempotent");
});
