//! `rtrt security` — scan workspaces and manage security profiles.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use rtrt_security::{
    BUILTIN_PROFILES, Finding, Profile, ScanReport, Severity, Standards, list_profiles,
    load_profile, user_profile_dir,
};

#[derive(Debug, Subcommand)]
pub enum SecurityCmd {
    /// Scan a directory with a security profile.
    Scan {
        /// Profile name.
        #[arg(long)]
        profile: String,
        /// Directory to scan.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Emit the full scan report as pretty JSON.
        #[arg(long)]
        json: bool,
    },
    /// List or inspect security profiles.
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Run a scan and exit non-zero when findings meet the profile threshold.
    Gate {
        /// Profile name.
        #[arg(long)]
        profile: String,
        /// Directory to scan.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Emit the full scan report as pretty JSON.
        #[arg(long)]
        json: bool,
    },
    /// Copy built-in profiles into the user profile directory.
    Init {
        /// Built-in profile name to copy. Omit to copy all built-ins.
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProfileCmd {
    /// List available profiles.
    List,
    /// Show one resolved profile.
    Show {
        /// Profile name.
        name: String,
    },
}

pub fn run(cmd: SecurityCmd) -> Result<()> {
    match cmd {
        SecurityCmd::Scan {
            profile,
            path,
            json,
        } => {
            let profile = load_profile(&profile)?;
            let report = scan(&profile, &path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report);
            }
            Ok(())
        }
        SecurityCmd::Profile { cmd } => match cmd {
            ProfileCmd::List => {
                let builtins: std::collections::BTreeSet<&str> =
                    BUILTIN_PROFILES.iter().map(|(name, _)| *name).collect();
                for name in list_profiles() {
                    if builtins.contains(name.as_str()) {
                        println!("{name} [builtin]");
                    } else {
                        println!("{name}");
                    }
                }
                Ok(())
            }
            ProfileCmd::Show { name } => {
                let profile = load_profile(&name)?;
                print_profile(&profile);
                Ok(())
            }
        },
        SecurityCmd::Gate {
            profile,
            path,
            json,
        } => {
            let profile = load_profile(&profile)?;
            let threshold = profile.severity_threshold;
            let report = scan(&profile, &path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            if report.fails_gate(threshold) {
                eprintln!(
                    "security gate failed: profile={} threshold={} counts={}",
                    report.profile,
                    threshold,
                    format_counts(&report.counts)
                );
                std::process::exit(1);
            }
            if !json {
                println!(
                    "OK: security gate passed profile={} threshold={} counts={}",
                    report.profile,
                    threshold,
                    format_counts(&report.counts)
                );
            }
            Ok(())
        }
        SecurityCmd::Init { profile } => init_profiles(profile.as_deref()),
    }
}

fn scan(profile: &Profile, path: &Path) -> Result<ScanReport> {
    rtrt_security::run(profile, path).with_context(|| {
        format!(
            "security scan {} with profile {}",
            path.display(),
            profile.name
        )
    })
}

fn init_profiles(name: Option<&str>) -> Result<()> {
    let dir = user_profile_dir().context("cannot resolve user profile directory")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;

    let selected: Vec<(&str, &str)> = match name {
        Some(name) => {
            let (_, text) = BUILTIN_PROFILES
                .iter()
                .find(|(profile_name, _)| *profile_name == name)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unknown built-in security profile: {name}"))?;
            vec![(name, text)]
        }
        None => BUILTIN_PROFILES.to_vec(),
    };

    for (name, text) in selected {
        let path = dir.join(format!("{name}.toml"));
        if path.exists() {
            continue;
        }
        std::fs::write(&path, text.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn print_report(report: &ScanReport) {
    println!(
        "security scan: profile={} root={} engines_run={} engines_skipped={}",
        report.profile,
        report.root,
        report.engines_run.len(),
        report.engines_skipped.len()
    );

    if report.findings.is_empty() {
        println!("findings: 0");
    } else {
        for severity in severity_desc() {
            let matches: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|finding| finding.severity == severity)
                .collect();
            if matches.is_empty() {
                continue;
            }
            println!("\n{}:", severity_label(severity));
            for finding in matches {
                print_finding(finding);
            }
        }
    }

    println!("\ncounts: {}", format_counts(&report.counts));
}

fn print_finding(finding: &Finding) {
    println!(
        "{:<8}  {}  {}  — {}",
        severity_label(finding.severity),
        finding.rule_id,
        location(finding),
        finding.title
    );
    if !finding.fix_hint.is_empty() {
        println!("  fix: {}", finding.fix_hint);
    }
    let standards = format_core_standards(&finding.standards);
    if !standards.is_empty() {
        println!("  standards: {standards}");
    }
}

fn print_profile(profile: &Profile) {
    println!("name: {}", profile.name);
    println!("description: {}", profile.description);
    println!("threshold: {}", profile.severity_threshold);
    if profile.exclude.is_empty() {
        println!("exclude: []");
    } else {
        println!("exclude:");
        for item in &profile.exclude {
            println!("  {item}");
        }
    }
    println!("rules:");
    for rule in &profile.rules {
        println!(
            "  {}  engine={} severity={} enabled={}",
            rule.id, rule.engine, rule.severity, rule.enabled
        );
        if !rule.description.is_empty() {
            println!("    description: {}", rule.description);
        }
        print_standards(&rule.standards, "    ");
    }
}

fn print_standards(standards: &Standards, indent: &str) {
    if standards.is_empty() {
        return;
    }
    println!("{indent}standards:");
    print_standard_field(indent, "CWE", &standards.cwe);
    print_standard_field(indent, "OWASP", &standards.owasp);
    print_standard_field(indent, "ASVS", &standards.asvs);
    print_standard_field(indent, "NIST", &standards.nist);
    print_standard_field(indent, "CIS", &standards.cis);
    print_standard_field(indent, "SLSA", &standards.slsa);
    print_standard_field(indent, "EU AI Act", &standards.eu_ai_act);
    print_standard_field(indent, "Other", &standards.other);
}

fn print_standard_field(indent: &str, label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    println!("{indent}  {label}: {}", values.join(", "));
}

fn format_core_standards(standards: &Standards) -> String {
    let mut parts = Vec::new();
    if !standards.cwe.is_empty() {
        parts.push(format!("CWE={}", standards.cwe.join(",")));
    }
    if !standards.owasp.is_empty() {
        parts.push(format!("OWASP={}", standards.owasp.join(",")));
    }
    if !standards.nist.is_empty() {
        parts.push(format!("NIST={}", standards.nist.join(",")));
    }
    parts.join(" ")
}

fn location(finding: &Finding) -> String {
    let file = if finding.file.is_empty() {
        "<project>"
    } else {
        finding.file.as_str()
    };
    match finding.line {
        Some(line) => format!("{file}:{line}"),
        None => file.to_string(),
    }
}

fn format_counts(counts: &rtrt_security::SeverityCounts) -> String {
    format!(
        "critical={} high={} medium={} low={} info={}",
        counts.critical, counts.high, counts.medium, counts.low, counts.info
    )
}

fn severity_desc() -> [Severity; 5] {
    [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Info,
    ]
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical => "CRITICAL",
        Severity::High => "HIGH",
        Severity::Medium => "MEDIUM",
        Severity::Low => "LOW",
        Severity::Info => "INFO",
    }
}
