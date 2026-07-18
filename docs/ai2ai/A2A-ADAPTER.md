# Agent2Agent (A2A) adapter boundary

Reviewed against the official A2A 1.0 specification on 2026-07-18:

- <https://a2a-protocol.org/latest/specification/>
- <https://github.com/a2aproject/A2A/blob/main/specification/a2a.proto>
- <https://a2a-protocol.org/latest/topics/agent-discovery/>

OpenSoverignBlog's `Ai2AiEnvelope` is an internal publishing command and audit
model. It is deliberately not named `Task`, `Message`, or `AgentCard`, and the
core does not claim A2A conformance.

An optional adapter may map the models as follows:

```text
OpenSoverignBlog capability       A2A representation
revision proposal                Task + Message with structured DataPart
context/provenance receipt       Artifact metadata or OSB extension Part
accepted revision               Artifact with content schema reference
long render/import operation     asynchronous Task
public capabilities             AgentSkill entries
OSB contract version             required AgentExtension URI
```

The adapter must implement a complete selected A2A binding, not merely publish
an attractive Agent Card. It must expose the official well-known
`/.well-known/agent-card.json` only when the declared interface, skills,
authentication, task lifecycle, cancellation, errors, and advertised optional
capabilities actually work.

Recommended OSB extension identifier:

```text
https://github.com/studyreadbook4ever/OpenSoverignBlog/tree/main/docs/ai2ai/a2a-extension/v1
```

The extension should carry only schema URLs, revision identifiers,
idempotency keys, content hashes, and policy references. Markdown or private
context must not be placed in a public Agent Card. Extended cards require the
authorization behavior defined by the A2A specification.

The core keeps HTTP API writes authoritative. The adapter authenticates the A2A
client, maps its principal into the same access-policy port, submits a normal
AI2AI proposal, and returns the resulting immutable revision artifact. It never
receives direct database access.
