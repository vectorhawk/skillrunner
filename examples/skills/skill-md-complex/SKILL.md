---
name: skill-md-complex
description: A reference skill exercising every AUTH1 extension — schemas, model requirements, and an out-of-line workflow.
license: Apache-2.0
vh_version: 1.0.0
vh_publisher: vectorhawk
vh_permissions:
  network: none
  filesystem: read-only
  clipboard: none
vh_execution:
  timeout_ms: 60000
  memory_mb: 1024
  sandbox: strict
vh_model:
  min_params_b: 7
  recommended:
    - llama3.1:8b
    - claude-3-haiku
  fallback: mcp_sampling
vh_schemas:
  inputs:
    type: object
    required:
      - passage
    properties:
      passage:
        type: string
        description: The passage of text to analyze.
      audience:
        type: string
        description: Optional audience hint (e.g. "beginner", "expert").
  outputs:
    type: object
    required:
      - summary
    properties:
      summary:
        type: string
        description: A one-paragraph summary of the passage.
      key_points:
        type: array
        items:
          type: string
        description: Bulleted list of key points.
vh_workflow_ref: ./workflow.yaml
---

# Passage Analyzer

You are a helpful assistant that analyzes a passage of text and produces a
short summary plus a list of key points, tailored to an optional audience
hint.

The workflow is defined in the sibling workflow.yaml file because it has
enough steps to warrant the escape hatch. Each step references a prompt
template in the sibling prompts/ directory.
