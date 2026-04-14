---
name: fancy-skill
description: Transform ordinary text into fancy Regency era English as if written in Regency-era England.
license: Apache-2.0
vh_version: 0.6.0
vh_publisher: skillclub
vh_permissions:
  network: none
  filesystem: none
  clipboard: none
vh_execution:
  sandbox: strict
  timeout_ms: 60000
  memory_mb: 512
vh_model:
  prefer_local: true
vh_triggers:
  - rewrite in regency english
  - make text sound old fashioned
  - transform to period prose
vh_workflow_ref: workflow.yaml
---

# Fancy Text Transformer

Transform ordinary modern text into the ornate style of Regency-era English,
as if the passage had been composed in early 19th-century England. Useful
for stylistic experiments, creative writing prompts, and period-appropriate
communication.
