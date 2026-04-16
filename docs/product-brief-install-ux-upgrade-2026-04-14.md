# Product Brief: VectorHawk Install UX Upgrade

**Created:** 2026-04-14
**Owner:** Adam Schneider
**BMAD Phase:** 1 (Analysis)
**Project Level:** 3

## Problem Statement

skills.sh shipped a best-in-class `npx skills add <url>` install experience for AI agent skills: zero-prereq install, multi-agent fan-out (45 clients), inline risk scores, clack-style prompts. VectorHawk's current `skill install` flow is functional but visually raw (raw `println!`, no multi-agent fan-out, no governance signals at install time). Our install UX is below the new baseline developers now expect.

## Target Users

- **Primary:** Individual developers installing VectorHawk skills via CLI (`vectorhawk skill install <id>`).
- **Secondary:** SKILL.md authors iterating locally, needing fast dev-loop install-from-source.
- **Tertiary:** Enterprise end users who should see governance context (publisher verified, org policy status, requested gateway scopes) before approving an install.

## Value Proposition

"skills.sh installs skills. VectorHawk governs them." Match skills.sh install polish, then leapfrog with pre-install governance signals that are structurally impossible for a non-governed tool to ship.

## Key Features

1. Visual polish (banner, clack-style prompts, summary boxes).
2. Symlink-vs-copy install mode (author dev-loop acceleration).
3. Multi-agent fan-out at install time (leveraging existing client detection in `skillrunner-mcp/src/setup.rs`).
4. Governance panel pre-install (publisher / policy / scopes / scan / audit).
5. Universal paste-to-import (GitHub URL, SKILL.md, npx, MCP JSON → registry classifier → `.cskill`).

## Success Metrics

- Install-flow NPS (or qualitative: "feels like skills.sh") from first 5 pre-alpha users.
- Time-to-first-skill for SKILL.md author on a fresh machine (target: < 60s).
- Governance-panel recognition: do enterprise reviewers notice it without prompting?
- Zero regression on CI / `--no-interactive` / `mcp serve` stdio paths.

## Constraints

- Pre-alpha, zero legacy users — breaking changes allowed.
- v5 enforcement model: client is convenience, not security boundary. Governance UX must not imply the client is enforcing (gateway enforces).
- macOS-first per existing CLAUDE.md; Windows support degrades gracefully.
- Phase 4 requires coordination with the separate `skillclub-registry/` repo (different codebase).

## Competitive Reference

Derived from competitive review of `npx skills add https://github.com/anthropics/skills --skill frontend-design` flow (skills.sh v1.5.0, observed 2026-04-14).
