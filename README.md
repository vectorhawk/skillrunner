# SkillRunner

The local runtime for [SkillClub](https://skillclub.ai) skills. A CLI-first Rust MVP for macOS that installs, validates, resolves, and executes portable AI skills.

## MVP scope
- macOS first (`arm64`, `x86_64`)
- CLI-first runtime (`skillrunner`)
- signed immutable `.skill` bundles
- structured workflow definitions
- central policy + silent updates
- local-first execution via a model broker

## Workspace crates
- `skillrunner-cli`: user-facing CLI binary (`skillrunner`)
- `skillrunner-core`: app state, runner services, policy, resolver, executor
- `skillrunner-manifest`: skill manifest, workflow, and schema parsing

## Current command surface
- `skillrunner doctor`
- `skillrunner skill import <SKILL.md>`
- `skillrunner skill info <path>`
- `skillrunner skill install <path>`
- `skillrunner skill list`
- `skillrunner skill resolve <skill-id>`
- `skillrunner skill run <skill-id> --input <file>`
- `skillrunner skill validate <path>`

## Verified baseline
- Manifest and workflow parsing for unpacked skills
- Import from SKILL.md frontmatter format
- Local app state bootstrap under macOS application support
- SQLite initialization for installed skill metadata
- Versioned install layout for unpacked local skills
- Policy client trait with mock implementation
- Skill resolver with version enforcement
- Input schema validation on run
- Full bundle validation command

## Example
```bash
cargo test
cargo run -p skillrunner-cli -- skill info ./examples/skills/contract-compare
cargo run -p skillrunner-cli -- skill validate ./examples/skills/contract-compare
cargo run -p skillrunner-cli -- skill install ./examples/skills/contract-compare
cargo run -p skillrunner-cli -- skill list
cargo run -p skillrunner-cli -- skill resolve contract-compare
cargo run -p skillrunner-cli -- skill run contract-compare --input input.json
cargo run -p skillrunner-cli -- doctor
```
