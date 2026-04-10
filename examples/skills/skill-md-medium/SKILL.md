---
name: skill-md-medium
description: A reference skill with an inline two-step workflow for summarizing text.
license: Apache-2.0
vh_version: 0.2.0
vh_publisher: vectorhawk
vh_permissions:
  network: registry
  filesystem: read-only
  clipboard: none
vh_execution:
  timeout_ms: 45000
  memory_mb: 768
  sandbox: strict
vh_workflow:
  - id: summarize
    type: llm
    prompt: prompts/summarize.md
  - id: shorten
    type: llm
    prompt: prompts/shorten.md
---

# Text Summarizer

You are a helpful assistant that summarizes passages of text into clear,
concise bullet points.

The workflow first runs a broad summarization pass, then a shortening pass
that tightens the bullets to one line each. Both passes use benign,
content-neutral prompts stored in the sibling prompts/ directory.
