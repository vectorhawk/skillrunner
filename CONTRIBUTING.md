# Contributing to SkillRunner

Thanks for your interest in contributing. SkillRunner is licensed Apache 2.0 and
developed under the [vectorhawk](https://github.com/vectorhawk) GitHub org.

## Filing Issues

Use [GitHub Issues](https://github.com/vectorhawk/skillrunner/issues). Include:
- Your OS and architecture (e.g. macOS ARM64)
- Steps to reproduce
- Expected vs. actual behavior
- Relevant output from `skillrunner doctor`

## Submitting Pull Requests

1. Fork the repo and create a branch from `main` (e.g. `fix/resolver-panic`)
2. Make your changes and ensure all checks pass (see below)
3. Open a PR against `main` with a clear description of what and why

Keep PRs focused — one logical change per PR. Large refactors should be discussed
in an issue first.

## Code Style and Checks

Before pushing, run:

```bash
cargo fmt
cargo clippy -- -D warnings
cargo test
```

All three must pass cleanly. PRs that fail clippy or have failing tests will not
be merged.

## Codebase Layout

SkillRunner is a four-crate Cargo workspace under `crates/`:

| Crate | Responsibility |
|---|---|
| `skillrunner-manifest` | Parses and validates skill bundles. Pure data, no I/O. |
| `skillrunner-core` | Business logic: install, resolve, execute, registry client, auth. |
| `skillrunner-mcp` | MCP server, aggregator, AI client setup. |
| `skillrunner-cli` | Clap command wiring and terminal output. |

Put new logic in `skillrunner-core` unless it is manifest parsing (manifest crate)
or purely CLI presentation (cli crate).

## Developer Certificate of Origin

This project uses the [DCO](https://developercertificate.org/) instead of a CLA.
Add a sign-off line to every commit:

```
Signed-off-by: Your Name <you@example.com>
```

You can do this automatically with `git commit -s`. By signing off you certify
that you wrote the contribution and have the right to submit it under the Apache
2.0 license.

## Questions

Open a GitHub Discussion or drop a comment on the relevant issue.
