# Architecture: VectorHawk Install UX Upgrade

**Created:** 2026-04-14
**BMAD Phase:** 3 (Solutioning)
**Project Level:** 3
**Related:** [prd-install-ux-upgrade-2026-04-14.md](./prd-install-ux-upgrade-2026-04-14.md)

## System Overview

Five-phase client/server change sequenced for earliest user value. Phases 1–3 are fully client-side in `skillrunner/`. Phase 4 adds one registry endpoint. Phase 5 extends an existing registry classifier. Each phase ships independently.

## Component Design

### New module: `skillrunner-cli/src/ui.rs`

Single UX home imported by every command that prompts or renders output.

```
pub fn banner()
pub fn summary_box(title: &str, rows: &[(String, String)])
pub fn confirm_box(title: &str, question: &str) -> bool
pub fn spinner(msg: &str) -> indicatif::ProgressBar
pub fn governance_panel(gov: &PreinstallGovernance)  // Phase 4
pub fn fanout_summary(report: &FanoutReport)         // Phase 3
pub fn theme() -> dialoguer::theme::ColorfulTheme
```

Guarded by `atty::is(Stream::Stdout)` and the existing `is_mcp_serve` check at `main.rs:336` so stdio JSON-RPC in `mcp serve` is never corrupted.

### Phase 1 — Visual polish

Touch `skillrunner-cli/src/main.rs`:
- `SkillCommands::Import` (lines 848–857): swap `stdin().read_line` for `dialoguer::Input` / `Confirm`.
- `SkillCommands::Author` (lines 904–931): same swap.
- `SkillCommands::Install` (line 1019): wrap success output in `ui::summary_box`.
- `main()` entry (line 331): emit `ui::banner()` under TTY guard.

### Phase 2 — Symlink install

Touch `skillrunner-core/src/install.rs`:
- New enum:
  ```rust
  pub enum InstallMode { Copy, Symlink }
  ```
- `install_unpacked_skill(..., mode: InstallMode)` — Symlink mode makes `versions/{ver}/` itself a symlink to source dir. `active/` logic unchanged.
- Record `source_type = 'local_symlink'` in `skill_versions` row.

Touch `skillrunner-cli/src/main.rs`:
- `Install.link: bool` flag, honored only if `is_local` (line 1025).

Touch `skillrunner-core/src/updater.rs`:
- `install_from_registry` pins `InstallMode::Copy` explicitly.

Windows: `#[cfg(target_family = "unix")]` gate; Windows returns clear error.

### Phase 3 — Multi-agent fan-out

Touch `skillrunner-mcp/src/setup.rs`:
- `pub fn client_skill_dir(client: &ClientConfig) -> Option<PathBuf>` — maps client kind to its user-skill dir (Claude Code, Cursor, Windsurf, Claude Desktop); returns `None` for clients that don't support skill dirs.
- `pub fn fanout_skill_to_clients(skill_id: &str, source: &Path, clients: &[&ClientConfig]) -> Result<FanoutReport>` — symlinks canonical `active/` into each client's skill dir (copy on Windows).
- `pub struct FanoutReport { installed: Vec<String>, skipped: Vec<(String, String)> }`.
- `pub fn fanout_uninstall_skill(skill_id: &str, clients: &[&ClientConfig])` — sweeps dangling links on `skill uninstall`.

Touch `skillrunner-cli/src/main.rs`:
- After successful install at lines 1032/1046: call `detect_ai_clients()`, render `MultiSelect` pre-checked with clients that have `client_skill_dir`, call `fanout_skill_to_clients()`.
- Flags: `--no-fanout`, `--clients=<csv>`.

Must-do research before shipping: verify Cursor and Windsurf support user-skill dirs today. If not, scope to Claude Code + Claude Desktop and mark others "MCP config only" in multi-select.

### Phase 4 — Governance panel

#### Registry additions (`skillclub-registry/`)

New endpoint `GET /api/runner/skills/{skill_id}/preinstall` in `backend/app/routers/runner.py`:

```
PreinstallGovernance {
  publisher: { id, name, verified: bool, verified_at }
  policy:    { status: "approved"|"pending"|"blocked", org_id, message }
  scopes_requested: [ { name, description, risk_level } ]
  scan:      { verdict: "clean"|"flagged"|"unknown", scanner, scanned_at, findings_count }
  audit:     { last_audited_at, auditor }
}
```

Aggregator only — data exists in `admin_publishers.py`, `admin_policies.py`, `admin_scanning.py`, `admin_audit.py`. New Pydantic schema at `backend/app/schemas/preinstall.py`. Tests at `backend/tests/test_preinstall_endpoint.py`.

Scan verdict returns `"unknown"` immediately when async scan hasn't completed — never block the endpoint.

#### Client additions

Touch `skillrunner-core/src/registry.rs`:
- `RegistryClient::fetch_preinstall_governance(skill_id) -> Result<PreinstallGovernance>` modeled on `fetch_skill_detail` (line 343).
- Matching `#[derive(Deserialize)]` structs.

Touch `skillrunner-cli/src/main.rs`:
- In `SkillCommands::Install` registry branch (line 1044): fetch preinstall → render `ui::governance_panel()` → `dialoguer::Confirm`.
- `policy.status = "blocked"` refuses. `pending` warns + requires `--force`.
- New `--dry-run` flag prints panel and exits (enables snapshot testing).

### Phase 5 — Universal paste-to-import

#### Registry additions

- Verify `POST /portal/import/preview` and `/submit` in `backend/app/routers/portal_import.py` accept all five input types. If pasted MCP JSON not yet covered, extend `services/import_service.py::classify_input` to detect `{"mcpServers":...}` or `{"command":...}` → `mcp_server` type.
- New runner-facing mirror `POST /api/runner/import/preview` (Bearer token auth instead of portal session cookies).

#### Client additions

Touch `skillrunner-core/src/import.rs`:
- Rename local helper → `import_local_skill_md` for clarity.
- New `import_via_registry(registry: &RegistryClient, raw_input: &str) -> Result<ImportOutcome>`.

```rust
pub enum ImportOutcome {
    SkillScaffolded { bundle: PathBuf },
    McpServerRequested { server_name: String, status: String },
    SkillSubmitted { submission_id: String, status: String },
}
```

Touch `skillrunner-cli/src/main.rs`:
- `SkillCommands::Import` (line 830): if path exists → local, else → registry classifier.
- Support `import -` for stdin paste.
- `--local` flag for offline-only scaffolding.

## Data Flow

### Phase 4 install flow
```
User: vectorhawk skill install contract-compare
  → RegistryClient::fetch_preinstall_governance(id)
  → GET /api/runner/skills/contract-compare/preinstall
  → Registry aggregates: publishers + policies + scan + audit tables
  → ui::governance_panel(gov) rendered
  → dialoguer::Confirm "Proceed?"
  → [approve] → existing install_from_registry()
  → [blocked] → hard refuse
  → [pending] → require --force
  → Phase 3 fan-out multi-select
  → ui::summary_box(FanoutReport)
```

### Phase 5 import flow
```
User: vectorhawk import <paste>
  → detect: file path? GitHub URL? raw MD? npx? JSON?
  → [file path] → import_local_skill_md()
  → [else]      → POST /api/runner/import/preview
                → registry classifier → preview
                → ui confirm
                → POST /api/runner/import/submit
                → ImportOutcome::{SkillScaffolded|McpServerRequested|SkillSubmitted}
  → ui::summary_box
```

## API Contracts

- `GET /api/runner/skills/{id}/preinstall` → `PreinstallGovernance` (new, Phase 4).
- `POST /api/runner/import/preview` → preview payload (new, Phase 5).
- `POST /api/runner/import/submit` → `ImportOutcome` payload (new, Phase 5).

## Test Strategy

| Phase | Unit | Integration | E2E |
|-------|------|-------------|-----|
| P0    | `insta` snapshots on `ui::*` helpers | — | — |
| 1     | — | — | Manual CLI smoke |
| 2     | `install.rs::install_symlink_mode_links_source_dir` | — | — |
| 3     | `setup.rs::client_skill_dir` per client (tempdir) | `tests/fanout.rs` with faked `HOME` | — |
| 4     | `registry.rs` with `mockito` (200/404/403) | `tests/preinstall.rs` end-to-end against mock server | Backend: `test_preinstall_endpoint.py` |
| 5     | `import.rs` with `mockito` (skill / mcp / 422) | `tests/import_paste.rs` | Backend classifier coverage |

## Technology Decisions

- **Dialoguer over custom prompts:** mature, Ctrl-C safe, theme-able. Matches clack aesthetic closely.
- **Symlink as canonical fan-out mechanism:** single source of truth at `~/Library/Application Support/SkillClub/SkillRunner/skills/{id}/active`; updates propagate without per-client rewrites.
- **Separate `/api/runner/*` endpoints over shared `/api/*`:** runner uses Bearer auth, portal uses session cookies. Keeping them separate avoids auth-scheme confusion and allows independent rate limits.
- **Scan verdict async-friendly:** `"unknown"` is a first-class value, never a blocking 202. Keeps preinstall endpoint fast.

## Security Considerations

- Client-side `--force` on `blocked` policy is UI-only; the v5 gateway still enforces. Help text must say this explicitly.
- `import` runner endpoint must validate that the caller's Bearer token has org context before running classifier (prevents anonymous abuse).
- Symlink fan-out writes into user-owned dirs only (`~/.claude/`, etc.); never system dirs.
- Pasted MCP JSON classified but not executed client-side — enforcement remains at gateway per v5.

## Open Tradeoffs

1. **Telemetry on decline.** Logging fan-out unchecks and governance declines helps product learning; conflicts with pre-alpha no-telemetry posture. Deferred.
2. **Cursor/Windsurf research.** Unblocks Phase 3 scope. Block shipping Phase 3 until verified.
3. **`--force` semantics.** Client-side only per v5. Make it loud in `--help` so users don't assume it bypasses enforcement.
