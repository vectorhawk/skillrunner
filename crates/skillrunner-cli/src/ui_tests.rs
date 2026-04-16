//! Snapshot tests for `ui.rs` formatting helpers.
//!
//! Uses `insta` for snapshot assertions.  After first run, accept with:
//!   cargo insta accept -p skillrunner-cli

#![allow(clippy::unwrap_used)]

use skillrunner_core::registry::{
    PreinstallAudit, PreinstallGovernance, PreinstallPolicy, PreinstallPublisher, PreinstallScan,
    PreinstallScope,
};

use super::{format_banner, format_governance_panel, format_summary_box};

/// Narrow rows — keys and values well within the 40-char minimum width.
#[test]
fn snapshot_summary_box_narrow() {
    let rows = vec![
        ("ID".to_string(), "contract-compare".to_string()),
        ("Version".to_string(), "0.2.0".to_string()),
        ("Status".to_string(), "installed".to_string()),
    ];
    let output = format_summary_box("Installed", &rows);
    insta::assert_snapshot!(output);
}

/// Wide rows — key+value pair exceeds the 40-char minimum, forcing the box to expand.
#[test]
fn snapshot_summary_box_wide() {
    let rows = vec![
        (
            "Publisher".to_string(),
            "vectorhawk-official-long-publisher-name".to_string(),
        ),
        (
            "Scope".to_string(),
            "read:files write:network execute:subprocess".to_string(),
        ),
        ("Risk".to_string(), "medium".to_string()),
    ];
    let output = format_summary_box("Governance", &rows);
    insta::assert_snapshot!(output);
}

/// Single row — exercises the minimum-width floor and edge-case of one entry.
#[test]
fn snapshot_summary_box_single_row() {
    let rows = vec![("Key".to_string(), "value".to_string())];
    let output = format_summary_box("Info", &rows);
    insta::assert_snapshot!(output);
}

/// Banner contains the crate version string and brand name.
#[test]
fn snapshot_banner() {
    let output = format_banner();
    // Structural checks — insta snapshot captures the exact string.
    assert!(output.contains("VectorHawk"), "banner must contain brand name");
    assert!(
        output.contains(env!("CARGO_PKG_VERSION")),
        "banner must contain crate version"
    );
    insta::assert_snapshot!(output);
}

// ─── governance_panel snapshots ───────────────────────────────────────────────

fn approved_governance() -> PreinstallGovernance {
    PreinstallGovernance {
        publisher: PreinstallPublisher {
            id: "pub-001".to_string(),
            name: "VectorHawk Official".to_string(),
            verified: true,
            verified_at: Some("2025-01-15T10:00:00Z".to_string()),
        },
        policy: PreinstallPolicy {
            status: "approved".to_string(),
            org_id: None,
            message: None,
        },
        scopes_requested: vec![
            PreinstallScope {
                name: "read:files".to_string(),
                description: Some("Read input documents".to_string()),
                risk_level: Some("low".to_string()),
            },
            PreinstallScope {
                name: "llm:call".to_string(),
                description: Some("Invoke a language model".to_string()),
                risk_level: Some("medium".to_string()),
            },
        ],
        scan: Some(PreinstallScan {
            verdict: "clean".to_string(),
            scanner: Some("vectorhawk-scanner".to_string()),
            scanned_at: Some("2025-12-01T08:00:00Z".to_string()),
            findings_count: Some(0),
        }),
        audit: Some(PreinstallAudit {
            last_audited_at: Some("2025-11-01T00:00:00Z".to_string()),
            auditor: Some("security-team".to_string()),
        }),
    }
}

/// All-green: approved policy, verified publisher, clean scan.
#[test]
fn snapshot_governance_panel_all_green() {
    let gov = approved_governance();
    let output = format_governance_panel(&gov);
    insta::assert_snapshot!(output);
}

/// Pending policy: unverified publisher, no scan yet.
#[test]
fn snapshot_governance_panel_pending_policy() {
    let gov = PreinstallGovernance {
        publisher: PreinstallPublisher {
            id: "pub-002".to_string(),
            name: "Community Publisher".to_string(),
            verified: false,
            verified_at: None,
        },
        policy: PreinstallPolicy {
            status: "pending".to_string(),
            org_id: None,
            message: Some("Awaiting org approval".to_string()),
        },
        scopes_requested: vec![PreinstallScope {
            name: "write:network".to_string(),
            description: None,
            risk_level: Some("high".to_string()),
        }],
        scan: None,
        audit: None,
    };
    let output = format_governance_panel(&gov);
    insta::assert_snapshot!(output);
}

/// Blocked policy: flagged scan with findings.
#[test]
fn snapshot_governance_panel_blocked_policy() {
    let gov = PreinstallGovernance {
        publisher: PreinstallPublisher {
            id: "pub-003".to_string(),
            name: "Untrusted Publisher".to_string(),
            verified: false,
            verified_at: None,
        },
        policy: PreinstallPolicy {
            status: "blocked".to_string(),
            org_id: Some("org-acme".to_string()),
            message: Some("Security policy violation detected".to_string()),
        },
        scopes_requested: vec![],
        scan: Some(PreinstallScan {
            verdict: "flagged".to_string(),
            scanner: Some("vectorhawk-scanner".to_string()),
            scanned_at: Some("2025-12-10T12:00:00Z".to_string()),
            findings_count: Some(3),
        }),
        audit: None,
    };
    let output = format_governance_panel(&gov);
    insta::assert_snapshot!(output);
}
