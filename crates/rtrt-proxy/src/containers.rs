use once_cell::sync::Lazy;
use regex::Regex;

use crate::{collapse_blanks, is_error_or_warning};

static DOCKER_PROGRESS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)^\s*([a-f0-9]{12}: )?(Pulling fs layer|Waiting|Downloading|Verifying Checksum|Download complete|Extracting|Pull complete|Already exists|Pushed|Preparing|Layer already exists)$",
    )
    .unwrap()
});

pub(crate) fn docker_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if (DOCKER_PROGRESS.is_match(trimmed)
            || trimmed.starts_with("#")
                && (trimmed.contains("CACHED")
                    || trimmed.contains("transferring context")
                    || trimmed.contains("DONE")))
            && !is_error_or_warning(trimmed)
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let collapsed = collapse_blanks(&out);
    if collapsed.trim().is_empty() && !input.trim().is_empty() {
        "ok\n".to_string()
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_drops_layer_progress_and_keeps_errors() {
        let raw = "abc123def456: Pulling fs layer\nabc123def456: Download complete\nerror: failed to solve: missing file\nSuccessfully tagged app:latest\n";
        let out = docker_filter(raw);
        assert!(!out.contains("Pulling fs layer"));
        assert!(!out.contains("Download complete"));
        assert!(out.contains("failed to solve"));
        assert!(out.contains("Successfully tagged"));
    }
}
