use crate::{collapse_blanks, is_error_or_warning};

pub(crate) fn kubectl_filter(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        let is_noise = trimmed.starts_with("Defaulted container ")
            || trimmed.starts_with("Waiting for deployment ")
            || trimmed == "deployment successfully rolled out";
        if is_noise && !is_error_or_warning(trimmed) {
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
    fn kubectl_drops_defaulted_container_noise_and_keeps_errors() {
        let raw = "Defaulted container \"app\" out of: app, sidecar\nError from server (NotFound): pods \"x\" not found\n";
        let out = kubectl_filter(raw);
        assert!(!out.contains("Defaulted container"));
        assert!(out.contains("NotFound"));
    }
}
