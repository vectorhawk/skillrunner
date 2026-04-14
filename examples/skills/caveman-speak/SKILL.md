---
name: caveman-speak
description: Compress text into caveman speak — strip articles, conjunctions, and filler while preserving meaning. Inspired by caveman-compression.
license: Apache-2.0
vh_version: 0.1.0
vh_publisher: skillclub

vh_permissions:
  network: none
  filesystem: none
  clipboard: none

vh_execution:
  sandbox: strict
  timeout_ms: 30000
  memory_mb: 256

# No vh_model block → prefer_local defaults to false.
# This skill runs on the calling AI client (Claude via MCP sampling)
# whether or not a local LLM is available.

vh_triggers:
  - convert to caveman speak
  - compress text
  - caveman compression
  - strip filler words

vh_workflow_ref: workflow.yaml
---

# Caveman Speak

Transform ordinary prose into "caveman speak" — a compressed form that
strips articles (a, the), conjunctions (and, but), auxiliaries
(is, are), and filler words while keeping the core meaning intact.

Inspired by [caveman-compression](https://github.com/wilpel/caveman-compression),
the technique can cut token count substantially in LLM conversations
while staying human-readable.

**Example:**
- Input: `"The cat sat on the mat and watched the bird fly away."`
- Output: `"cat sit mat watch bird fly away"`

This skill has no `vh_model.prefer_local` flag set, so it uses
whatever LLM is calling it (typically Claude via MCP sampling).
