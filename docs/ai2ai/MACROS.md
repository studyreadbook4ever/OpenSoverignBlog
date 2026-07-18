# Inert AI macro workflow

OpenSoverignBlog supports a provider-neutral macro unit without embedding an
LLM in Studio or executing model output during page requests. A macro is JSON
inside an ordinary Markdown code fence:

````markdown
```osb-ai-macro
{
  "specVersion": "1.0",
  "invocationId": "01900000-0000-7000-8000-000000000001",
  "macroId": "org.example.expand-outline",
  "definitionVersion": "1.0.0",
  "phase": "draft",
  "inputs": { "section": "security boundary" },
  "actor": { "kind": "agent", "id": "local-writer" },
  "policy": {
    "dataBoundary": "local_only",
    "allowedCapabilities": ["content.propose"],
    "maxCost": 0,
    "maxTokens": 1200
  },
  "contextReceipts": [],
  "requiresReview": true,
  "createdAt": "2026-07-18T00:00:00Z"
}
```
````

The block is portable and inert. A normal Markdown renderer displays it as
code. The Rust kernel can parse bounded blocks, reject duplicate IDs, validate
policy snapshots, verify resolution hashes, and replace reviewed blocks. It
does not call OpenAI, Anthropic, Google, a local model, a shell, or a plugin by
itself.

## Safe sequence

```text
author or agent writes inert block
→ adapter discovers and validates macro definition + requested capabilities
→ operator-selected provider receives only context marked included
→ adapter returns a hashed Markdown fragment
→ required human review is recorded
→ kernel applies resolution to a complete Markdown document
→ agent submits that document through the normal AI2AI base-revision endpoint
→ publishing remains a separate authorized action
```

Use [`macro-invocation.v1.schema.json`](../../schemas/macro-invocation.v1.schema.json)
for the wire shape and [`macros.rs`](../../crates/kernel/src/macros.rs) for the
reference parser/application rules. Plugin manifests declare macro IDs, input
and output schemas, phases, cache policy, and review requirements. Provider
credentials and conversation memory never belong inside the Markdown block.

The reference server currently exposes the AI2AI revision endpoint, not an
embedded provider dispatcher or durable macro job queue. Provider adapters are
therefore external/capability-scoped; this avoids silently turning every blog
installation into an LLM client. A later dispatcher must add durable jobs,
budget accounting, retry/idempotency, secret bindings, and audit artifacts
without changing the portable block shape.
