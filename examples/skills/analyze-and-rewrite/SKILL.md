---
name: analyze-and-rewrite
description: Analyze the tone of a piece of text, then rewrite it in a more appropriate tone for its intended audience.
license: Apache-2.0
vh_version: 0.1.0
vh_publisher: skillclub

vh_permissions:
  network: none
  filesystem: none
  clipboard: none

vh_execution:
  sandbox: strict
  timeout_ms: 120000
  memory_mb: 1024

vh_model:
  min_params_b: 7.0
  recommended:
    - llama3.2:8b
    - mistral:7b
  fallback: mcp_sampling
  prefer_local: true

vh_triggers:
  - analyze and rewrite this text
  - improve the tone of this text
  - fix this text for a different audience

vh_workflow_ref: workflow.yaml
---

# Analyze and Rewrite

Two-step skill that first analyzes the tone of the input text (formal,
casual, aggressive, hedging, etc.) and then rewrites it for the
intended audience.

Demonstrates a multi-step workflow where step 2 consumes the structured
output of step 1. Both steps run with `prefer_local: true`, so the
entire pipeline stays on the user's machine when Ollama is available.

**Inputs:**
- `text` — the text to analyze and rewrite
- `target_audience` — short description of who the rewrite is for
  (e.g. `"executive summary for the board"`, `"Slack message to a teammate"`)

**Outputs:**
- `analysis` — structured tone analysis from step 1
- `rewrite` — the rewritten text from step 2
