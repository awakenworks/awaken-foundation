//! Foundation guardrails.
//!
//! `guardrail-lints` enforces the one invariant the foundation tier exists to
//! keep: a foundation crate never depends on a product or component *above* it.
//! Foundation is the bottom of the stack — `awaken-iam`, `oversight-*`, and the
//! products depend on it, never the reverse — so any `awaken-*` / `oversight-*`
//! dependency that is not itself a foundation crate is a layering violation.

use std::path::Path;
use std::process::ExitCode;

/// Crates that legitimately live in this workspace; deps on these are fine.
const FOUNDATION_CRATES: &[&str] = &[
    "awaken-api-contract",
    "awaken-connection",
    "awaken-connection-auth",
    "awaken-connection-transports",
    "awaken-scoped-migration",
    "xtask",
];

fn main() -> ExitCode {
    match std::env::args().nth(1).unwrap_or_default().as_str() {
        "guardrail-lints" => guardrail_lints(),
        other => {
            eprintln!("unknown task: {other:?}; expected `guardrail-lints`");
            ExitCode::FAILURE
        }
    }
}

fn guardrail_lints() -> ExitCode {
    let mut violations = Vec::new();
    for dir in ["crates", "xtask"] {
        scan(Path::new(dir), &mut violations);
    }
    if violations.is_empty() {
        println!(
            "guardrail-lints: no upward (product/component) dependency in any foundation crate"
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("guardrail-lints: foundation must not depend on a product/component above it:");
        for v in &violations {
            eprintln!("  {v}");
        }
        ExitCode::FAILURE
    }
}

fn scan(dir: &Path, violations: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            scan(&path, violations);
        } else if path.file_name().is_some_and(|n| n == "Cargo.toml") {
            check_manifest(&path, violations);
        }
    }
}

fn check_manifest(path: &Path, violations: &mut Vec<String>) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut in_deps = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line.contains("dependencies");
            continue;
        }
        if !in_deps || line.is_empty() || line.starts_with('#') {
            continue;
        }
        // dependency name is the token before the first `=`, `.`, or whitespace.
        let name = line
            .split(['=', '.', ' ', '\t'])
            .next()
            .unwrap_or("")
            .trim();
        let is_org = name.starts_with("awaken-") || name.starts_with("oversight-");
        if is_org && !FOUNDATION_CRATES.contains(&name) {
            violations.push(format!(
                "{}: depends on `{name}` (above the foundation tier)",
                path.display()
            ));
        }
    }
}
