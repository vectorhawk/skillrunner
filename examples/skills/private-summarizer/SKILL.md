---
name: private-summarizer
description: Summarize text using a local language model so the input never leaves the user's machine.
license: Apache-2.0
vh_version: 0.1.0
vh_publisher: skillclub

vh_permissions:
  network: none
  filesystem: none
  clipboard: none

vh_execution:
  sandbox: strict
  timeout_ms: 60000
  memory_mb: 1024

vh_model:
  min_params_b: 7.0
  recommended:
    - llama3.2:8b
    - gemma3:4b
  fallback: mcp_sampling
  prefer_local: true

vh_triggers:
  - summarize this privately
  - summarize text locally
  - summarize without cloud

vh_workflow_ref: workflow.yaml
---

# Private Summarizer

This skill summarizes text using the user's locally-running language
model (via Ollama) so the source text is never sent to a cloud provider.

Use this when privacy or compliance matters: customer records, internal
documents, medical notes, draft contracts. The `prefer_local: true`
setting on `vh_model` tells the runtime to dispatch to Ollama first. If
no local model is available the runtime falls back to MCP sampling —
that fallback can be disabled by setting `fallback: error` if strict
locality is required.

The summary is a one-paragraph digest (2–4 sentences) along with a word
count of the original input.
