# Architecture: Installation Scopes (User vs Project)

**Created:** 2026-04-16
**BMAD Phase:** 3 (Solutioning)
**Related:** [prd-install-ux-upgrade-2026-04-14.md](./prd-install-ux-upgrade-2026-04-14.md)

## Overview

Two install scopes: user (global, existing) and project (repo-local, new). Project scope uses a lockfile + gitignored cache, modeled after npm/cargo. No org or team scopes for pre-alpha.

## Lockfile: `.vectorhawk/skills.lock.json`

```json
{
  "version": 1,
  "skills": {
    "contract-compare": {
      "version": "1.2.0",
      "source": "registry",
      "registry_url": "https://app.vectorhawk.ai",
      "integrity": "sha256-abc123..."
    },
    "my-custom-skill": {
      "source": "local",
      "local_path": "./skills/my-custom-skill.md"
    }
  }
}
```

Committed to git. Registry URL stored for reproducibility across teams using different registries.

## Cache: `.vectorhawk/skills/`

Compiled bundles / registry downloads. Gitignored (auto-generated `.vectorhawk/.gitignore` with `skills/` entry on first project install). Equivalent to `node_modules/`.

## CLI surface

- `skillrunner skill install <ref> --project [PATH]` — project scope
- `skillrunner skill install <ref> --user` — user scope (explicit)
- `skillrunner skill install <ref>` (no flag, TTY) — interactive scope picker
- `skillrunner skill install <ref>` (no flag, non-TTY) — user scope (backwards compat)
- `skillrunner install` (bare) — reads lockfile, restores all project skills
- `skillrunner skill uninstall <id> --project` — removes from lockfile + cache
- `skillrunner skill list` — shows scope column, project shadows user

## Precedence

Project scope shadows user scope for same skill ID. `resolve_skill` checks `.vectorhawk/skills/{id}/active` first, falls through to user DB.

## Offline behavior

- Registry skill cached from prior run → use silently, warn it's from cache
- Registry skill not cached (first clone) → fail that skill, continue others
- Local SKILL.md → always works (compiled locally via `import_local_skill_md`)

## Phases

### Phase 1: Lockfile types + IO
New `crates/skillrunner-core/src/lockfile.rs`. `Lockfile` struct, `LockedSkill` enum (Registry/Local), `load/save/discover` methods. `discover` walks ancestors looking for `.vectorhawk/skills.lock.json`.

### Phase 2: InstallScope enum + project install path
`InstallScope { User, Project(path) }` in `install.rs`. New `install_project_skill` copies bundle to `.vectorhawk/skills/{id}/`, upserts lockfile, saves. Auto-generates `.vectorhawk/.gitignore`.

### Phase 3: `skillrunner install` (bare restore)
Iterates lockfile entries. Registry: check cache + integrity → skip or download. Local: compile SKILL.md via `import_local_skill_md`. Partial failure: report individual errors, don't abort batch.

### Phase 4: Scoped resolution (project shadows user)
`resolve_skill` gains optional `project_root` param. Project cache checked first. `ResolveOutcome::Active` gets `scope` field. Update callers in MCP tools + CLI.

### Phase 5: Interactive picker scope step + CLI flags
`--user`, `--project [PATH]` flags. TTY scope picker. Wire into uninstall (`--project`) and list (scope column).

### Phase 6: Integrity verification on restore
`verify_integrity(cache_path, expected)` using sha256. Mismatch triggers re-download. `--offline` makes mismatch a hard error.
