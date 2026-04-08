# SkillRunner

A local runtime for portable AI skills and an MCP aggregator that connects your AI coding tools to every backend they need.

SkillRunner sits between your AI client (Claude Code, Cursor, Windsurf, VS Code, Gemini CLI) and the MCP servers you depend on. It manages tool namespacing, enforces a tool budget so clients don't choke, and lets you define your entire backend stack in a single `backends.yaml` file.

It also has its own skill format -- portable, versioned bundles of prompts and workflows that execute locally via Ollama or delegate to MCP sampling.

## Install

### Homebrew

```bash
brew install skillclub/tap/skillrunner
```

### Pre-built binaries

Binaries for macOS (arm64, x86_64) and Linux (x86_64, arm64) are attached to each [GitHub release](https://github.com/skillclub/skillrunner/releases).

```bash
# macOS Apple Silicon
curl -L https://github.com/skillclub/skillrunner/releases/latest/download/skillrunner-aarch64-apple-darwin.tar.gz | tar xz
sudo mv skillrunner /usr/local/bin/
```

### Build from source

Requires Rust 1.75+.

```bash
git clone https://github.com/skillclub/skillrunner.git
cd skillrunner
cargo build --release
cp target/release/skillrunner /usr/local/bin/
```

## Quick start

```bash
# Check your setup
skillrunner doctor

# Set up SkillRunner as an MCP server for your AI clients
skillrunner mcp setup

# Start the MCP server (usually managed by your AI client after setup)
skillrunner mcp serve
```

## MCP aggregator

The aggregator is the main reason to use SkillRunner. It proxies multiple MCP backend servers through a single stdio connection, with automatic tool namespacing to prevent collisions:

```
github__create_issue     <- GitHub MCP server
sentry__search_issues    <- Sentry MCP server
playwright__screenshot   <- Playwright MCP server
```

### Configure backends

Create `~/Library/Application Support/SkillClub/SkillRunner/backends.yaml`:

```yaml
backends:
  - name: GitHub
    transport: stdio
    command: npx
    args: ["-y", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "ghp_xxxx"

  - name: Sentry
    server_id: sentry
    transport: http
    url: http://localhost:3001/mcp
    priority: 60

  - name: Playwright
    transport: stdio
    command: npx
    args: ["-y", "@anthropic-ai/mcp-server-playwright"]
```

Each backend entry supports:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Display name |
| `transport` | yes | `stdio` or `http` |
| `command` | stdio | Command to run |
| `args` | stdio | Command arguments |
| `env` | stdio | Environment variables |
| `url` | http | Server URL |
| `server_id` | no | Override the auto-generated ID used for namespacing |
| `priority` | no | Higher = more important when the tool budget is tight (default: 50) |

### Tool budget

AI clients like Cursor and Windsurf cap tools at 100. SkillRunner enforces this limit, allocating slots by priority and truncating lower-priority backends when the budget runs out.

### AI client setup

```bash
# Auto-detect and configure all supported clients
skillrunner mcp setup

# Configure a specific client
skillrunner mcp setup --client claude
skillrunner mcp setup --client cursor
```

Supported clients: Claude Code, Cursor, Windsurf, VS Code (Copilot), Gemini CLI.

## Skills

Skills are portable AI tool bundles -- a directory containing a manifest, workflow, prompts, and schemas.

### Author a skill from a prompt

The fastest way to create a skill:

```bash
skillrunner skill import ./my-skill.md
```

Where `my-skill.md` is a Markdown file with YAML frontmatter:

```markdown
---
id: code-reviewer
version: 0.1.0
publisher: yourname
model: llama3.2
---

You are a senior code reviewer. Analyze the provided code diff and return
structured feedback covering correctness, performance, and style issues.
```

This scaffolds a complete skill bundle with manifest, workflow, prompt, and schemas.

### Run a skill

```bash
# With a local model (requires Ollama)
echo '{"diff": "..."}' > input.json
skillrunner skill run code-reviewer --input input.json

# Without a model (stub mode, for testing the workflow)
skillrunner skill run code-reviewer --input input.json --stub
```

### Skill bundle structure

```
my-skill/
  manifest.json     # id, version, publisher, permissions, model requirements
  workflow.yaml     # ordered steps: llm, tool, transform, validate
  schemas/
    inputs.json     # JSON Schema for input validation
    outputs.json    # JSON Schema for output validation
  prompts/
    system.txt      # prompt templates referenced by workflow steps
```

### Manage skills

```bash
skillrunner skill list                    # list installed skills
skillrunner skill install ./path/to/skill # install from local directory
skillrunner skill validate ./path/to/skill # validate without installing
skillrunner skill resolve my-skill        # check install status and policy
```

## Architecture

Four-crate Rust workspace:

| Crate | Role |
|-------|------|
| `skillrunner-cli` | User-facing CLI (clap) |
| `skillrunner-core` | State management, installer, resolver, executor, policy, Ollama client |
| `skillrunner-manifest` | Skill bundle parsing and validation (pure data, no I/O) |
| `skillrunner-mcp` | MCP server, aggregator, AI client setup, tool dispatch |

### Feature flags

The `registry` feature (default: on) enables SkillClub registry integration -- auth, remote policy, auto-updates, governance tools. Disable it to build a fully standalone binary:

```bash
# Library crates compile without registry support
cargo build -p skillrunner-core --no-default-features
cargo build -p skillrunner-mcp --no-default-features
```

## Development

```bash
cargo build
cargo test
cargo clippy
cargo run -p skillrunner-cli -- doctor
```

## Requirements

- macOS 13+ or Linux (x86_64, arm64)
- [Ollama](https://ollama.ai) for local LLM execution (optional -- skills can also delegate via MCP sampling)

## License

Apache 2.0. See [LICENSE](LICENSE).
