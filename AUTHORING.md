# Authoring VectorHawk Skills

> Draft — AUTH1a phase. This document describes the SKILL.md-rooted authoring
> format. The Rust runtime (AUTH1b), the registry compile step (AUTH1c), and
> the CLI ergonomics (AUTH1d) that consume this format are not yet shipped.

## What is a VectorHawk skill?

A VectorHawk skill is a directory whose root contains a single `SKILL.md`
file. That file has a YAML frontmatter block at the top (metadata + VectorHawk
extensions) followed by a Markdown body that serves as the skill's primary
prompt.

VectorHawk uses the Anthropic Agent Skills format as-is for the standard
fields (`name`, `description`, `license`) and layers `vh_`-prefixed extensions
for VectorHawk-specific metadata (permissions, execution constraints,
workflows, I/O schemas, model requirements).

Optional siblings live next to `SKILL.md` in the same directory:

```
my-skill/
|-- SKILL.md          # canonical entry point (required)
|-- prompts/          # additional prompt templates referenced by workflow steps
|-- scripts/          # auxiliary scripts (Anthropic standard)
|-- references/       # reference documents (Anthropic standard)
|-- assets/           # static assets (Anthropic standard)
`-- workflow.yaml     # referenced by vh_workflow_ref, for complex workflows
```

Three reference examples live in `examples/skills/`:

- `skill-md-minimal/` — metadata only, no workflow, no schemas
- `skill-md-medium/` — inline `vh_workflow` with sibling `prompts/`
- `skill-md-complex/` — `vh_workflow_ref` + inline schemas + model requirements

## Frontmatter fields

The full contract is the JSON Schema at
`crates/skillrunner-manifest/schemas/skill_md_frontmatter.json`. What follows
is a narrative summary; defer to the schema file for the authoritative rules.

### Standard Anthropic fields (required)

| Field | Required | Purpose |
|---|---|---|
| `name` | yes | Skill handle, kebab-case. Becomes the canonical identifier. |
| `description` | yes | One-sentence summary used by AI clients to decide when to invoke. |
| `license` | yes | SPDX license identifier. **Required** — without it, the Cisco scanner emits a `MANIFEST_MISSING_LICENSE` INFO finding on every publish. |

### VectorHawk extensions

All VectorHawk extensions use the `vh_` prefix. **Unknown `vh_*` fields are
rejected** at compile time by the registry. Typos and stale authoring guides
surface as hard errors, not silent drops.

| Field | Required | Purpose |
|---|---|---|
| `vh_version` | yes | Semver version of the skill. |
| `vh_publisher` | yes | Publisher id. |
| `vh_permissions` | no | `network` / `filesystem` / `clipboard` scopes. Registry injects org defaults for missing sub-fields. |
| `vh_execution` | no | `timeout_ms` / `memory_mb` / `sandbox` constraints. Registry injects org defaults. |
| `vh_model` | no | `min_params_b` / `recommended[]` / `fallback`. |
| `vh_schemas` | no | Inline JSON Schemas for `inputs` and `outputs`. |
| `vh_workflow` | no | Inline list of workflow steps. Mutually exclusive with `vh_workflow_ref`. |
| `vh_workflow_ref` | no | Path to a sibling `workflow.yaml` file. Mutually exclusive with `vh_workflow`. |

### The `vh_*` namespace rule

- `vh_*` keys not in the explicit allowlist above → **rejected**
- Non-`vh_*` keys not in the Anthropic standard set → **passed through**; the
  AUTH1c compile step logs a warning so we can track upstream Anthropic spec
  additions (`author`, `tags`, `requires`, etc.) without breaking builds

### `vh_workflow` vs `vh_workflow_ref` — which to use?

Inline `vh_workflow` keeps a simple skill in one file. For complex workflows,
inline YAML gets unwieldy and awkward to diff.

**Rule of thumb**: use `vh_workflow_ref: ./workflow.yaml` when either of these
is true:

- **5 or more steps**, OR
- **30 or more lines of inline YAML** for the workflow block

Below those thresholds, inline is fine. Above either threshold, move it to
a sibling `workflow.yaml`.

## Minimal complete example

This is `examples/skills/skill-md-minimal/SKILL.md` verbatim:

```markdown
---
name: skill-md-minimal
description: A minimal reference skill that greets the user politely.
license: Apache-2.0
vh_version: 0.1.0
vh_publisher: vectorhawk
---

# Minimal Greeter

You are a helpful assistant that responds to any greeting with a friendly,
short reply and wishes the user a productive day.

Keep every reply to one sentence. Do not discuss topics outside of greetings.
```

That's the entire skill. No `prompts/` directory, no `workflow.yaml`, no
schemas. The Markdown body below the frontmatter is the skill's primary
prompt.

## Registry defaults (AUTH1c)

The AUTH1c registry compile step (not yet shipped) will inject org policy
defaults for any missing `vh_permissions` or `vh_execution` sub-fields and
will reject any frontmatter that requests permissions stricter than the org's
allowed maximum. Authors can omit these blocks entirely and let org defaults
apply, or specify them explicitly to pin a tighter contract.

`vh_model` has no default injection — it's either declared or absent.

## Verification

During AUTH1a the three reference examples were empirically verified against
`cisco-ai-skill-scanner==1.0.2` running in the VectorHawk sidecar. All three
return `is_safe: true`, `max_severity: SAFE`, zero findings, and the scanner
emits no warnings about `vh_*` extensions. See the AUTH1a report for the
captured scanner output.
