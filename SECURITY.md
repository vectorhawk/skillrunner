# Security Policy

## Reporting a Vulnerability

Please report security vulnerabilities to **security@vectorhawk.io**. Do not open a public GitHub issue for security matters.

We will acknowledge your report within **48 hours** and aim to deliver a fix within **90 days**, depending on severity and complexity. We will keep you informed of progress throughout the process.

## Supported Versions

Only the **latest release** receives security fixes. We encourage all users to stay on the current release.

| Version | Supported |
|---------|-----------|
| Latest  | Yes       |
| Older   | No        |

## Scope

**In scope:**

- The `skillrunner` binary (CLI and daemon modes)
- The MCP server (`mcp serve`) and its JSON-RPC message handling
- The MCP aggregator and tool-budget enforcement
- Skill execution (process spawning, input/output handling, schema validation)
- Registry client (authentication, artifact download, policy enforcement)

**Out of scope:**

- Third-party MCP servers proxied through the aggregator
- Ollama or any other locally installed model runtime
- AI client integrations (Claude Code, Cursor, Windsurf, VS Code, Gemini CLI)
- The SkillClub registry service itself (separate codebase)

## Preferred Report Contents

- Description of the vulnerability and potential impact
- Steps to reproduce or a minimal proof-of-concept
- Affected version(s) and platform(s)
- Any suggested mitigations, if known
