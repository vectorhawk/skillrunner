# SkillRunner

The local runtime for [SkillClub](https://skillclub.ai) skills. Installs, validates, resolves, and executes portable AI skills on macOS — locally, privately, and under your organization's policy.

## Installation

### Homebrew (recommended)

```bash
brew install skillclub/tap/skillrunner
```

> The Homebrew tap is coming soon. Star this repo to be notified.

### Download a pre-built binary

Signed binaries for macOS `arm64` and `x86_64` are published with each release.

```bash
# Apple Silicon
curl -L https://releases.skillclub.ai/skillrunner/latest/skillrunner-aarch64-apple-darwin \
  -o /usr/local/bin/skillrunner && chmod +x /usr/local/bin/skillrunner

# Intel
curl -L https://releases.skillclub.ai/skillrunner/latest/skillrunner-x86_64-apple-darwin \
  -o /usr/local/bin/skillrunner && chmod +x /usr/local/bin/skillrunner
```

Verify your download:
```bash
skillrunner doctor
```

### Enterprise / MDM deployment

For IT teams deploying to a fleet:

- A signed `.pkg` installer is available per release — suitable for deployment via Jamf, Mosyle, or Kandji.
- The binary installs to `/usr/local/bin/skillrunner` and stores state under `~/Library/Application Support/SkillClub/SkillRunner/`.
- Set `SKILLCLUB_REGISTRY_URL` in your managed environment profile to point all users at your organization's private registry.

### Build from source

Requires [Rust](https://rustup.rs) 1.75+.

```bash
git clone https://github.com/skillclub/skillrunner
cd skillrunner
cargo build --release
cp target/release/skillrunner /usr/local/bin/
```

Or via `cargo install`:
```bash
cargo install --git https://github.com/skillclub/skillrunner skillrunner-cli
```

---

## Requirements

- macOS 13+ (`arm64` or `x86_64`)
- [Ollama](https://ollama.ai) running locally (for `llm` step execution)

```bash
# Pull a model before running skills
ollama pull llama3.2
```

---

## Quick start

```bash
# Verify installation
skillrunner doctor

# Install a skill
skillrunner skill install ./examples/skills/contract-compare

# Run it
echo '{"doc_a": "Payment due Jan 1.", "doc_b": "Payment due Mar 1."}' > /tmp/input.json
skillrunner skill run contract-compare --input /tmp/input.json

# Run without a model (stub mode, for testing)
skillrunner skill run contract-compare --input /tmp/input.json --stub
```

---

## Commands

| Command | Description |
|---|---|
| `skillrunner doctor` | Verify runtime state and paths |
| `skillrunner skill install <path>` | Install a skill bundle from a local directory |
| `skillrunner skill import <SKILL.md>` | Scaffold a skill bundle from a Markdown file |
| `skillrunner skill list` | List all installed skills |
| `skillrunner skill info <path>` | Show metadata for a skill bundle |
| `skillrunner skill validate <path>` | Validate a skill bundle without installing |
| `skillrunner skill resolve <skill-id>` | Check install status and policy for a skill |
| `skillrunner skill run <skill-id> --input <file>` | Execute a skill with a JSON input file |

**`skill run` flags:**

| Flag | Default | Description |
|---|---|---|
| `--input <file>` | _(required)_ | JSON input file |
| `--stub` | off | Skip model calls; trace execution without LLM |
| `--model <name>` | `llama3.2` | Ollama model to use |
| `--ollama-url <url>` | `http://localhost:11434` | Ollama base URL |
| `--registry-url <url>` | _(env var)_ | Override registry for policy and auto-update |

Set `SKILLCLUB_REGISTRY_URL` in your environment to connect to your organization's registry without passing `--registry-url` on every command.

---

## Workspace

| Crate | Role |
|---|---|
| `skillrunner-cli` | User-facing CLI binary |
| `skillrunner-core` | App state, executor, policy, resolver, installer |
| `skillrunner-manifest` | Skill manifest, workflow, and schema parsing |

---

## Development

```bash
cargo build
cargo test
cargo clippy
cargo run -p skillrunner-cli -- doctor
```
