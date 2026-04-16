//! Shared UX helpers for the VectorHawk CLI.
//
// The public API is intentionally unused in this crate until Phase 1 (task #2)
// wires it into commands.  Suppress the lint here rather than on each item.
#![allow(dead_code)]
//!
//! All rendering functions are guarded by [`is_tty`], which returns `false`
//! when stdout is not a TTY *or* when the process is running as an MCP stdio
//! server.  This guarantees that nothing in this module ever writes to stdout
//! in a way that could corrupt the JSON-RPC transport used by `mcp serve`.
//!
//! The `IS_MCP_SERVE` flag is set once at startup via [`set_mcp_serve`] before
//! any helper is called, so the guard is always accurate.

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm};
use indicatif::{ProgressBar, ProgressStyle};
use skillrunner_core::registry::PreinstallGovernance;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set to `true` by `main()` when running as `mcp serve`.
static IS_MCP_SERVE: AtomicBool = AtomicBool::new(false);

/// Called once from `main()` before any UI function is used.
pub fn set_mcp_serve(val: bool) {
    IS_MCP_SERVE.store(val, Ordering::Relaxed);
}

/// Returns `true` when it is safe to render interactive or ANSI output on stdout.
///
/// Conditions that make this `false`:
/// - stdout is not a TTY (piped, redirected, etc.)
/// - the process is running as `mcp serve` (stdio JSON-RPC transport)
pub fn is_tty() -> bool {
    !IS_MCP_SERVE.load(Ordering::Relaxed) && atty::is(atty::Stream::Stdout)
}

/// Returns the shared `ColorfulTheme` used for all `dialoguer` prompts.
pub fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

/// Renders a compact VectorHawk ASCII banner to stdout.
///
/// No-ops when [`is_tty`] returns `false`.
pub fn banner() {
    if !is_tty() {
        return;
    }
    let hawk = style("VectorHawk").bold().cyan();
    let ver = style(env!("CARGO_PKG_VERSION")).dim();
    println!();
    println!("  {}  {}", hawk, ver);
    println!(
        "  {}",
        style("Governed AI skills for your team").dim()
    );
    println!();
}

/// Renders a bordered summary box with left-aligned keys and right-aligned values.
///
/// `title` is printed as the box header. Each `(key, value)` pair occupies one
/// row.  The box width is determined by the widest row (minimum 40 chars).
///
/// No-ops when [`is_tty`] returns `false`.
///
/// # Example
/// ```
/// ui::summary_box("Installed", &[
///     ("ID".to_string(), "contract-compare".to_string()),
///     ("Version".to_string(), "0.2.0".to_string()),
/// ]);
/// ```
pub fn summary_box(title: &str, rows: &[(String, String)]) {
    if !is_tty() {
        return;
    }
    // Determine column widths.
    let key_width: usize = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let val_width: usize = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    // Inner width = key + " : " + value, minimum 40.
    let inner = (key_width + 3 + val_width).max(40);

    let top = format!("  ╭─ {} {}", title, "─".repeat(inner.saturating_sub(title.len() + 1)));
    let bot = format!("  ╰{}", "─".repeat(inner + 2));

    println!("{}", style(top).dim());
    for (key, val) in rows {
        let padding = inner - key_width - 3 - val.len();
        println!(
            "  {} {:<kw$} : {}{} {}",
            style("│").dim(),
            style(key).bold(),
            " ".repeat(padding),
            style(val).cyan(),
            style("│").dim(),
            kw = key_width,
        );
    }
    println!("{}", style(bot).dim());
    println!();
}

/// Renders an inline confirmation prompt.
///
/// Returns `true` if the user confirms, `false` otherwise.
/// Returns `false` without prompting when [`is_tty`] returns `false`.
pub fn confirm_box(title: &str, question: &str) -> bool {
    if !is_tty() {
        return false;
    }
    println!("  {}", style(title).bold());
    Confirm::with_theme(&theme())
        .with_prompt(question)
        .default(false)
        .interact()
        .unwrap_or(false)
}

/// Creates an indeterminate spinner with `msg` as the prefix.
///
/// The caller is responsible for calling `.finish_and_clear()` or
/// `.finish_with_message()` when the operation completes.
///
/// When [`is_tty`] returns `false` the spinner is returned in a hidden state
/// so callers can always use the same API without branching.
pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    if !is_tty() {
        pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        return pb;
    }
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

// ─── Governance panel ─────────────────────────────────────────────────────────

/// Renders the pre-install governance panel to stdout.
///
/// Shows publisher verification status, policy status (color-coded),
/// requested scopes, scan verdict, and audit info.
///
/// When [`is_tty`] returns `false` the panel is rendered as plain text without
/// ANSI codes and written to stdout — this is intentional so that non-TTY
/// dry-run invocations (e.g. CI scripts) still receive machine-readable output.
pub fn governance_panel(gov: &PreinstallGovernance) {
    let text = format_governance_panel(gov);
    print!("{text}");
}

/// Pure-string version of [`governance_panel`] for snapshot testing.
///
/// Renders without ANSI codes so snapshots are readable and stable.
pub fn format_governance_panel(gov: &PreinstallGovernance) -> String {
    let mut out = String::new();

    // Header
    out.push_str("\n  ── Governance ──────────────────────────────────\n");

    // Publisher line
    let verified_mark = if gov.publisher.verified {
        "✓ verified"
    } else {
        "⚠ unverified"
    };
    out.push_str(&format!(
        "  Publisher : {} [{}]\n",
        gov.publisher.name, verified_mark
    ));

    // Policy line
    let policy_label = match gov.policy.status.as_str() {
        "approved" => "approved",
        "pending" => "pending",
        "blocked" => "BLOCKED",
        other => other,
    };
    let org_note = match &gov.policy.org_id {
        Some(id) => format!("org: {id}"),
        None => "org context not yet enforced".to_string(),
    };
    out.push_str(&format!(
        "  Policy    : {policy_label} ({org_note})\n"
    ));
    if let Some(msg) = &gov.policy.message {
        out.push_str(&format!("              {msg}\n"));
    }

    // Scopes
    if gov.scopes_requested.is_empty() {
        out.push_str("  Scopes    : none requested\n");
    } else {
        out.push_str("  Scopes    :\n");
        for scope in &gov.scopes_requested {
            let risk = scope
                .risk_level
                .as_deref()
                .unwrap_or("unknown");
            out.push_str(&format!("    • {} [risk: {risk}]\n", scope.name));
        }
    }

    // Scan
    match &gov.scan {
        None => out.push_str("  Scan      : not yet completed\n"),
        Some(scan) => {
            let verdict_label = match scan.verdict.as_str() {
                "clean" => "clean",
                "flagged" => "FLAGGED",
                "unknown" => "unknown (scan pending)",
                other => other,
            };
            let count_note = match scan.findings_count {
                Some(0) => " (0 findings)".to_string(),
                Some(n) => format!(" ({n} findings)"),
                None => String::new(),
            };
            out.push_str(&format!(
                "  Scan      : {verdict_label}{count_note}\n"
            ));
        }
    }

    // Audit
    match &gov.audit {
        None => out.push_str("  Audit     : no audit on record\n"),
        Some(audit) => {
            let when = audit.last_audited_at.as_deref().unwrap_or("unknown");
            let by = audit.auditor.as_deref().unwrap_or("unknown");
            out.push_str(&format!("  Audit     : last audited {when} by {by}\n"));
        }
    }

    out.push_str("  ────────────────────────────────────────────────\n\n");
    out
}

/// Renders a color-coded governance panel to stdout when connected to a TTY.
///
/// Falls back to plain-text rendering (via [`format_governance_panel`]) for
/// non-TTY contexts. Callers should use this over [`governance_panel`] when
/// they want ANSI highlights in interactive terminals.
pub fn governance_panel_tty(gov: &PreinstallGovernance) {
    if !is_tty() {
        governance_panel(gov);
        return;
    }

    println!();
    println!("  {}", style("── Governance ──────────────────────────────────").dim());

    // Publisher line
    let verified_mark = if gov.publisher.verified {
        style("✓ verified").green().to_string()
    } else {
        style("⚠ unverified").yellow().to_string()
    };
    println!(
        "  {} : {} [{}]",
        style("Publisher").bold(),
        gov.publisher.name,
        verified_mark
    );

    // Policy line
    let policy_styled = match gov.policy.status.as_str() {
        "approved" => style("approved").green().to_string(),
        "pending" => style("pending").yellow().to_string(),
        "blocked" => style("BLOCKED").red().bold().to_string(),
        other => style(other).dim().to_string(),
    };
    let org_note = match &gov.policy.org_id {
        Some(id) => format!("org: {id}"),
        None => style("org context not yet enforced").dim().to_string(),
    };
    println!(
        "  {} : {policy_styled} ({org_note})",
        style("Policy   ").bold()
    );
    if let Some(msg) = &gov.policy.message {
        println!("             {}", style(msg).dim());
    }

    // Scopes
    if gov.scopes_requested.is_empty() {
        println!("  {} : none requested", style("Scopes   ").bold());
    } else {
        println!("  {} :", style("Scopes   ").bold());
        for scope in &gov.scopes_requested {
            let risk = scope.risk_level.as_deref().unwrap_or("unknown");
            let risk_styled = match risk {
                "low" => style(risk).green().to_string(),
                "medium" => style(risk).yellow().to_string(),
                "high" | "critical" => style(risk).red().to_string(),
                other => style(other).dim().to_string(),
            };
            println!("    {} {} [risk: {risk_styled}]", style("•").dim(), scope.name);
        }
    }

    // Scan
    match &gov.scan {
        None => {
            println!("  {} : {}", style("Scan     ").bold(), style("not yet completed").dim());
        }
        Some(scan) => {
            let verdict_styled = match scan.verdict.as_str() {
                "clean" => style("clean").green().to_string(),
                "flagged" => style("FLAGGED").red().bold().to_string(),
                "unknown" => style("unknown (scan pending)").yellow().to_string(),
                other => style(other).dim().to_string(),
            };
            let count_note = match scan.findings_count {
                Some(0) => style(" (0 findings)").dim().to_string(),
                Some(n) => style(format!(" ({n} findings)")).yellow().to_string(),
                None => String::new(),
            };
            println!(
                "  {} : {verdict_styled}{count_note}",
                style("Scan     ").bold()
            );
        }
    }

    // Audit
    match &gov.audit {
        None => {
            println!("  {} : {}", style("Audit    ").bold(), style("no audit on record").dim());
        }
        Some(audit) => {
            let when = audit.last_audited_at.as_deref().unwrap_or("unknown");
            let by = audit.auditor.as_deref().unwrap_or("unknown");
            println!(
                "  {} : last audited {when} by {by}",
                style("Audit    ").bold()
            );
        }
    }

    println!("  {}", style("────────────────────────────────────────────────").dim());
    println!();
}

// ─── Internal helpers exposed for testing ─────────────────────────────────────

/// Pure-string version of [`summary_box`] used by snapshot tests.
///
/// Produces the same output as `summary_box` but returns it as a `String`
/// rather than writing to stdout, so tests can capture it without a TTY.
pub fn format_summary_box(title: &str, rows: &[(String, String)]) -> String {
    let key_width: usize = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let val_width: usize = rows.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    let inner = (key_width + 3 + val_width).max(40);

    let top = format!("  ╭─ {} {}", title, "─".repeat(inner.saturating_sub(title.len() + 1)));
    let bot = format!("  ╰{}", "─".repeat(inner + 2));

    let mut out = String::new();
    out.push_str(&top);
    out.push('\n');
    for (key, val) in rows {
        let padding = inner - key_width - 3 - val.len();
        out.push_str(&format!(
            "  │ {:<kw$} : {}{} │\n",
            key,
            " ".repeat(padding),
            val,
            kw = key_width,
        ));
    }
    out.push_str(&bot);
    out.push('\n');
    out
}

/// Pure-string version of the banner for snapshot testing.
pub fn format_banner() -> String {
    let ver = env!("CARGO_PKG_VERSION");
    format!(
        "\n  VectorHawk  {ver}\n  Governed AI skills for your team\n\n",
        ver = ver,
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
