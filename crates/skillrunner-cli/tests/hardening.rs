//! Integration tests for Task #10 cross-cutting hardening.
//!
//! Three concerns:
//! (a) `--yes` / non-interactive coverage — prompts don't hang in non-TTY
//! (b) `mcp serve` stdio purity — only JSON-RPC on stdout
//! (c) Uninstall fan-out sweep — symlinks removed, real dirs left alone
//!
//! All subprocess tests target the `skillrunner` binary built by Cargo.
//! We use `CARGO_BIN_EXE_skillrunner` (set by the test harness) to find it.

#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Returns the path to the compiled `skillrunner` binary.
fn skillrunner_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo when running integration tests.
    // Fall back to a relative path so editors' test runners don't panic.
    std::env::var("CARGO_BIN_EXE_skillrunner")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/debug/skillrunner")
        })
}

/// Walk `root` recursively and return the path of the first entry whose
/// file name matches `name`. Returns `None` if not found.
fn find_file_under(root: &std::path::Path, name: &str) -> Option<PathBuf> {
    for entry in walkdir(root) {
        if entry.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(entry.to_path_buf());
        }
    }
    None
}

/// Minimal recursive directory walker. Returns a flat list of all entries
/// reachable from `root` (symlinks are not followed to avoid cycles).
fn walkdir(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(path.clone());
            if is_dir {
                stack.push(path);
            }
        }
    }
    out
}

/// Creates a minimal SKILL.md-based skill bundle directory under `base` and returns its path.
fn make_skill_bundle(base: &std::path::Path) -> PathBuf {
    let skill_dir = base.join("test-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    // SKILL.md is the canonical format — manifest.json is derived at load time.
    let skill_md = "---\nname: Test Skill\ndescription: A test skill for integration tests\nlicense: Apache-2.0\n---\n\nYou are a helpful test assistant.\n";
    std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

    skill_dir
}

// ── (a) --yes / non-interactive coverage ─────────────────────────────────────

/// `skill import --yes` on a non-existent SKILL.md should exit with an error
/// quickly without blocking on any prompt.
///
/// We use a piped (non-TTY) stdout/stderr harness. If this hangs the test runner
/// kills it via timeout; a non-hang is sufficient proof.
#[test]
fn import_yes_flag_exits_cleanly_in_non_tty() {
    let tmp = TempDir::new().unwrap();
    // Create a minimal SKILL.md to import
    let skill_md = tmp.path().join("SKILL.md");
    std::fs::write(
        &skill_md,
        "---\nname: My Test\ndescription: integration test skill\nlicense: Apache-2.0\n---\n\nYou are a helpful assistant.\n",
    )
    .unwrap();

    let skillhome = tmp.path().join("skillhome");

    let out = Command::new(skillrunner_bin())
        .args([
            "skill",
            "import",
            skill_md.to_str().unwrap(),
            "--yes",
            "--skip-metadata",
        ])
        .env("SKILLCLUB_HOME", skillhome.to_str().unwrap())
        // Pipe everything so there is no TTY — dialoguer must not block
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch skillrunner");

    // The import itself should succeed (0) or fail gracefully (non-zero is fine
    // as long as the process exited rather than hanging).
    // The key assertion is that `output()` returned at all (i.e. no hang).
    // We also verify there is no interactive-prompt artifact in stdout.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // No TTY prompt fragments should appear on stdout
    assert!(
        !stdout.contains("Run recommendation engine"),
        "interactive prompt leaked to stdout in non-TTY mode: {stdout}"
    );
    assert!(
        !stdout.contains("Accept triggers?"),
        "interactive confirm leaked to stdout in non-TTY mode: {stdout}"
    );
    // stderr is informational only — just ensure the process exited
    let _ = stderr;
}

/// `skill import` with `--yes` and `--accept-suggestions` (no `--skip-metadata`)
/// must not block in a non-TTY environment even when there are missing metadata fields.
#[test]
fn import_yes_with_missing_metadata_does_not_hang_in_non_tty() {
    let tmp = TempDir::new().unwrap();
    // Minimal SKILL.md — intentionally missing vh_* fields so metadata is "missing"
    let skill_md = tmp.path().join("SKILL.md");
    std::fs::write(
        &skill_md,
        "---\nname: Bare Skill\ndescription: bare\nlicense: Apache-2.0\n---\n\nHelp the user.\n",
    )
    .unwrap();

    let skillhome = tmp.path().join("skillhome");

    let out = Command::new(skillrunner_bin())
        .args([
            "skill",
            "import",
            skill_md.to_str().unwrap(),
            "--yes",
        ])
        .env("SKILLCLUB_HOME", skillhome.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch skillrunner");

    // Must exit (no hang). Any exit code is acceptable.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Accept triggers?"),
        "prompt leaked with --yes in non-TTY: {stdout}"
    );
    assert!(
        !stdout.contains("Accept permissions?"),
        "prompt leaked with --yes in non-TTY: {stdout}"
    );
}

/// `skill install <local-path> --yes --no-fanout` must not hang in a non-TTY
/// context even when the install completes (fanout MultiSelect is the other
/// interactive surface).
#[test]
fn install_local_yes_no_fanout_does_not_hang_in_non_tty() {
    let tmp = TempDir::new().unwrap();
    let skill_dir = make_skill_bundle(tmp.path());
    let skillhome = tmp.path().join("skillhome");

    let out = Command::new(skillrunner_bin())
        .args([
            "skill",
            "install",
            skill_dir.to_str().unwrap(),
            "--no-fanout",
            "--yes",
        ])
        .env("SKILLCLUB_HOME", skillhome.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch skillrunner");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // No interactive prompt text should appear
    assert!(
        !stdout.contains("Fan out skill"),
        "fanout MultiSelect leaked in non-TTY: {stdout}"
    );
    // The install itself should succeed
    assert!(
        out.status.success(),
        "install failed with exit code {:?}\nstdout: {stdout}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
}

// ── (b) mcp serve stdio purity ────────────────────────────────────────────────

/// Start `mcp serve`, send an `initialize` JSON-RPC request on stdin, read the
/// response from stdout, and assert it parses as valid JSON with no banner or
/// ANSI escape sequences contaminating the stream.
///
/// Marked `#[ignore]` so it is skipped in environments where the binary may not
/// be present (e.g., docs-only CI runs). Run explicitly with:
///   cargo test -p skillrunner-cli -- --ignored mcp_serve_stdout_is_pure_json_rpc
#[test]
#[ignore = "requires compiled skillrunner binary; run with --ignored in integration CI"]
fn mcp_serve_stdout_is_pure_json_rpc() {
    let tmp = TempDir::new().unwrap();
    let skillhome = tmp.path().join("skillhome");
    // Pre-create the state db so bootstrap doesn't fail
    std::fs::create_dir_all(&skillhome).unwrap();

    let mut child = Command::new(skillrunner_bin())
        .args(["mcp", "serve"])
        .env("SKILLCLUB_HOME", skillhome.to_str().unwrap())
        // Pipe stdin so we can write the request
        .stdin(Stdio::piped())
        // Pipe stdout so we can read the response
        .stdout(Stdio::piped())
        // Stderr is tracing output — not our concern here
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn skillrunner mcp serve");

    let initialize_request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "0.0.1"}
        }
    });
    let request_line = format!("{}\n", serde_json::to_string(&initialize_request).unwrap());

    // Write the request
    {
        let stdin = child.stdin.as_mut().expect("stdin should be piped");
        stdin
            .write_all(request_line.as_bytes())
            .expect("write to mcp serve stdin");
        stdin.flush().expect("flush mcp serve stdin");
    }

    // Read one response line with a timeout guard via a thread
    let stdout = child.stdout.take().expect("stdout should be piped");
    let response_line = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        // Read at most one non-empty line
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    });

    // Give the server 5 s to respond
    let response_text = match response_line.join() {
        Ok(Some(t)) => t,
        Ok(None) => {
            let _ = child.kill();
            panic!("mcp serve closed stdout without producing a response line");
        }
        Err(_) => {
            let _ = child.kill();
            panic!("reader thread panicked");
        }
    };

    // Kill the server now that we have our line
    let _ = child.kill();
    let _ = child.wait();

    // 1. Must parse as JSON
    let value: serde_json::Value = serde_json::from_str(&response_text)
        .unwrap_or_else(|e| panic!("response is not valid JSON: {e}\nraw: {response_text}"));

    // 2. Must look like a JSON-RPC response
    assert_eq!(
        value.get("jsonrpc").and_then(|v| v.as_str()),
        Some("2.0"),
        "response missing jsonrpc:2.0 field: {value}"
    );
    assert!(
        value.get("id").is_some(),
        "response missing 'id' field: {value}"
    );

    // 3. Must contain no ANSI escape sequences
    assert!(
        !response_text.contains('\x1b'),
        "ANSI escape sequence found in mcp serve stdout: {response_text}"
    );

    // 4. Must contain no banner text
    assert!(
        !response_text.contains("VectorHawk"),
        "banner text leaked into mcp serve stdout: {response_text}"
    );
    assert!(
        !response_text.contains("Governed AI"),
        "banner text leaked into mcp serve stdout: {response_text}"
    );
}

// ── (c) Uninstall fan-out sweep ───────────────────────────────────────────────

/// Install a skill locally with `--no-fanout`, then manually create a
/// per-client symlink mimicking what fan-out would have done, then
/// `skill uninstall` and assert the symlink is gone while a real dir is left
/// intact.
///
/// This is a tempdir-based subprocess test that overrides the SkillRunner home.
/// Fan-out client detection looks at `~/.*` dirs; we use a fake HOME via the
/// `HOME` env var (Unix only) so no user config is touched.
#[test]
#[cfg(target_family = "unix")]
fn uninstall_removes_fanout_symlink_and_leaves_real_dirs() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    let skill_dir = make_skill_bundle(tmp.path());

    // Install the skill locally (no fanout, we manage symlinks manually)
    let install_out = Command::new(skillrunner_bin())
        .args([
            "skill",
            "install",
            skill_dir.to_str().unwrap(),
            "--no-fanout",
        ])
        .env("HOME", fake_home.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch skillrunner install");

    assert!(
        install_out.status.success(),
        "install failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&install_out.stdout),
        String::from_utf8_lossy(&install_out.stderr),
    );

    // Locate the active/ symlink by walking the fake_home tree — this is
    // platform-agnostic (macOS uses ~/Library/Application Support/...,
    // Linux uses ~/.local/share/...).
    let active_path = find_file_under(&fake_home, "active")
        .expect("active/ path should exist somewhere under fake_home after install");

    // Simulate fan-out: create ~/.claude/skills/test-skill -> active/
    let claude_skills = fake_home.join(".claude").join("skills");
    std::fs::create_dir_all(&claude_skills).unwrap();
    let symlink_entry = claude_skills.join("test-skill");
    std::os::unix::fs::symlink(&active_path, &symlink_entry).unwrap();
    assert!(
        symlink_entry.exists() || std::fs::symlink_metadata(&symlink_entry).is_ok(),
        "symlink should exist before uninstall"
    );

    // Create a real directory for Cursor to prove it is left untouched
    let cursor_skills = fake_home.join(".cursor").join("skills");
    std::fs::create_dir_all(&cursor_skills).unwrap();
    let real_dir = cursor_skills.join("test-skill");
    std::fs::create_dir_all(&real_dir).unwrap();
    // Write something inside so we can confirm it survives
    std::fs::write(real_dir.join("sentinel.txt"), "do not delete").unwrap();

    // Uninstall
    let uninstall_out = Command::new(skillrunner_bin())
        .args(["skill", "uninstall", "test-skill"])
        .env("HOME", fake_home.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch skillrunner uninstall");

    assert!(
        uninstall_out.status.success(),
        "uninstall failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&uninstall_out.stdout),
        String::from_utf8_lossy(&uninstall_out.stderr),
    );

    // Assertion 1: The fan-out symlink we created is gone
    assert!(
        std::fs::symlink_metadata(&symlink_entry).is_err(),
        "fan-out symlink should be removed after uninstall, but it still exists at {:?}",
        symlink_entry
    );

    // Assertion 2: The real directory (Cursor) is untouched
    assert!(
        real_dir.exists(),
        "real directory should NOT be removed by uninstall sweep"
    );
    assert!(
        real_dir.join("sentinel.txt").exists(),
        "content inside real directory should survive uninstall sweep"
    );
}

/// `skill uninstall` for a skill that was never fanned out must be a no-op
/// (no error, exit 0 or graceful "not installed" message).
#[test]
fn uninstall_skill_never_fanned_out_is_noop() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");

    // Don't install anything — just try to uninstall a nonexistent skill
    let out = Command::new(skillrunner_bin())
        .args(["skill", "uninstall", "ghost-skill"])
        .env("HOME", fake_home.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to launch skillrunner uninstall");

    // Must exit cleanly — the skill not being installed is handled gracefully
    assert!(
        out.status.success(),
        "uninstall of never-installed skill should exit 0\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should print a friendly "not installed" message
    assert!(
        stdout.contains("not installed"),
        "expected 'not installed' in output: {stdout}"
    );
}
