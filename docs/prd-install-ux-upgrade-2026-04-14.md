# PRD: VectorHawk Install UX Upgrade

**Created:** 2026-04-14
**BMAD Phase:** 2 (Planning)
**Project Level:** 3
**Related:** [product-brief-install-ux-upgrade-2026-04-14.md](./product-brief-install-ux-upgrade-2026-04-14.md)

## Executive Summary

Upgrade VectorHawk's `skill install` / `skill import` / `mcp setup` CLI UX to match skills.sh polish, then differentiate with a pre-install governance panel and universal paste-to-import. Delivered as five independently-shippable phases over ~2 sprints, sequenced for earliest user-visible value.

## User Stories

**US-1 (Polish):** As a developer running `vectorhawk skill install`, I see a banner, clack-style prompts, and a summary box, so the tool feels production-grade.

**US-2 (Symlink):** As a SKILL.md author iterating locally, I can `vectorhawk skill install ./my-skill --link` and see my edits reflected immediately through `active/`, without re-installing.

**US-3 (Fan-out):** As a developer with multiple AI clients installed, when I install a skill I get a multi-select of detected clients (Claude Code, Cursor, Windsurf, Claude Desktop) and the skill lands in each.

**US-4 (Governance):** As an enterprise user, before confirming an install I see publisher-verified status, org policy (approved/pending/blocked), gateway scopes the skill will request, last audit timestamp, and any scan verdict.

**US-5 (Import-anything):** As any user, I can run `vectorhawk import <paste>` and the system detects whether it's a GitHub URL, raw SKILL.md, npx command, or MCP JSON — then routes through the registry classifier and governance gate.

## Functional Requirements

### FR-1: UX foundations (P0)
- Add `dialoguer`, `console`, `indicatif` as workspace deps.
- New module `skillrunner-cli/src/ui.rs` with `banner()`, `summary_box()`, `confirm_box()`, `spinner()`, themed prompts.
- All prompts TTY-gated; `mcp serve` stdout stays pristine.

### FR-2: Visual polish (Phase 1)
- Replace raw `println!` / `stdin().read_line` in `SkillCommands::Import`, `::Author`, `::Install` with dialoguer + `ui::summary_box`.
- Banner emitted on CLI entry when TTY and subcommand allows.
- `--no-interactive` / `--yes` flags on every new prompt (follow existing `mcp setup --auto` precedent).

### FR-3: Install mode (Phase 2)
- New `InstallMode { Copy, Symlink }` enum in `install.rs`.
- `--link` flag on `skill install`, honored only for local-path installs.
- Registry installs forced to `Copy` mode in `updater.rs::install_from_registry`.
- Windows returns clear error for `--link`.

### FR-4: Multi-agent fan-out (Phase 3)
- `fanout_skill_to_clients()` in `skillrunner-mcp/src/setup.rs`.
- Post-install multi-select prompt pre-checked with detected clients that support user-skill dirs.
- Flags: `--no-fanout`, `--clients=claude,cursor,...`.
- `FanoutReport` rendered via `ui::summary_box`.
- Uninstall sweeps dangling fan-out symlinks.

### FR-5: Governance panel (Phase 4)
- New registry endpoint `GET /api/runner/skills/{id}/preinstall` aggregating publisher / policy / scopes / scan / audit.
- `RegistryClient::fetch_preinstall_governance()` in `skillrunner-core/src/registry.rs`.
- `ui::governance_panel()` rendered before confirm prompt.
- `blocked` refuses install. `pending` warns and requires `--force` (client-side bypass only — gateway still enforces).
- `scan.verdict` may return `"unknown"` immediately; backend async.

### FR-6: Universal import (Phase 5)
- `vectorhawk import <input>` detects GitHub URL / raw SKILL.md / npx string / MCP JSON / local path.
- Routes through registry `/api/runner/import/preview` + `/submit` (runner-facing mirror of existing portal endpoints).
- `ImportOutcome` enum: `SkillScaffolded`, `McpServerRequested`, `SkillSubmitted`.
- `--local` flag forces local-only SKILL.md scaffolding (offline path preserved).
- `import -` reads stdin for paste flows.

## Non-Functional Requirements

- **Zero stdio corruption in `mcp serve`.** All UX guarded by TTY check.
- **CI-safe.** Every new interactive prompt has `--yes` / `--no-interactive` equivalent.
- **No regressions** on existing `skill install`, `skill import`, `mcp setup --auto` paths.
- **Cross-platform baseline:** macOS (primary), Linux (best-effort), Windows (graceful degradation).

## Acceptance Criteria

- Phase 1: user running `cargo run -p skillrunner-cli -- skill install <local-path>` sees banner + dialoguer prompts + summary box.
- Phase 2: `--link` against a local skill creates a symlink; editing source file is live through `active/`.
- Phase 3: with Claude Code + Cursor installed, install fan-out multi-select appears pre-checked; skill appears in both clients' skill dirs.
- Phase 4: registry returns `PreinstallGovernance` JSON; CLI renders panel before confirm; `blocked` status refuses install.
- Phase 5: `vectorhawk import <github-url>` succeeds; `vectorhawk import -` with pasted MCP JSON classifies correctly.

## Out of Scope (v1)

- Telemetry on decline rates (privacy decision deferred).
- Windows `--link` support.
- Portal UI changes (this is CLI-only).
- Scan pipeline changes in registry (backend consumes existing scan results).

## Open Questions

1. Do Cursor / Windsurf support user-skill directories today, or MCP-config-only? Research blocks Phase 3.
2. Should `blocked` policy be overridable by org admin via `--force` or only by backend role? Per v5 enforcement model, backend decides — `--force` is UI-only.
3. Telemetry privacy: log decline events or not?

## Shipping Order

1. Phase 1 (polish) — 1–2 days, client-only, no API dependency.
2. Phase 2 (symlink) — 2 days, client-only.
3. Phase 3 (fan-out) — 3 days, client-only, needs Cursor/Windsurf research first.
4. Phase 4 (governance panel) — 3–4 days, requires registry endpoint.
5. Phase 5 (universal import) — 3 days, cross-repo, ships behind feature flag.
