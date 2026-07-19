# ADR 0003: Semantic Redis and managed on-premise operations

Status: accepted

## Decision

OpenSoverignBlog keeps SQLite and content-addressed blobs authoritative and
offers Redis as a selectable public hot-path module. `none` serves directly
from the origin, `redis_standalone` adds one cache node, and `redis_managed`
adds replica/Sentinel discovery. Deployment configuration is a versioned
intent graph rather than independent booleans. Supported intents are
`personal`, `community`, and `delivery`; impossible combinations fail before
the server opens a listener.

When selected, public response caching uses a release-scoped Redis namespace
and non-repeating generation tokens. Reads remember the generation on miss and
can fill only that same generation. A cancellation-safe mutation guard
suspends cache reads for the whole canonical write and leaves a newer dirty
ticket on every exit; the response path or next reader then rotates Redis.
Generation publication is compare-and-exchange protected, so an older Redis
reply cannot clear a newer invalidation, and loss of the generation key cannot
resurrect a prior cache. Redis errors invoke origin fallback, record a degraded
dependency state, discard the connection manager, and cause Sentinel master
discovery on reconnection. With Redis absent, these layers are not constructed
and public requests read the same authoritative repository directly.

Each cached envelope is HMAC-SHA256 authenticated with a bootstrap-generated
application key that is never passed to Redis. The signature binds the route,
generation, response allowlist, and body, preventing a writable or corrupted
Redis instance from forging or transplanting a public response. Oversized
values are rejected by Lua with `STRLEN` before `GET` transfers them.

The recommended interactive Compose choice intentionally spends extra local
resources on a primary, replica, and three Sentinel voters. Its purpose is
zero-touch process recovery, not a false claim of host-level high availability.
Small hosts may select no Redis or one standalone node. Every profile obtains
host/disk recovery from automatic verified SQLite/blob generations written to
an independently mountable backup destination.

Redis uses its native hybrid persistence instead of an application loop that
periodically unloads and reloads memory: AOF is fsynced every second, an RDB
checkpoint is produced after the configured change threshold, and the replica
keeps a second hot copy. A bespoke memory shuttle would add an ambiguous second
write protocol and a crash-consistency window without making canonical content
safer. Redis remains rebuildable from SQLite/blobs after a total cache loss.
The bundled nodes and Sentinels require a bootstrap-generated password; Redis
ports are never published to the host.

## Invariants

- Redis never contains the sole copy of content, identity, authorization, or a
  draft.
- An installation with `cache=none` contains no Redis credential or Redis
  readiness requirement and remains a supported public origin.
- No application-managed memory-to-disk loop is part of the durability
  contract; Redis AOF/RDB and canonical backup generations have distinct jobs.
- A Redis outage may increase latency but cannot grant access or change the
  selected revision.
- Private and mutation responses are never cached.
- Redis cache bodies are not trusted without the application-only integrity
  signature, and only verified hits increment the hit metric.
- Different immutable delivery releases use different cache namespaces.
- Managed backup retention can delete only validated generation directories
  under its dedicated root.
- Delivery nodes never migrate or mutate their SQLite artifact.
- `osb.intent.json` contains no secret and is sufficient for an agent to
  explain the intended topology before changing it.

## Consequences

Local development can select `cache=none` and needs no Redis process. The
recommended managed stack is less resource-efficient than a single cache
container, but routine cache-process failover requires no operator command.
Losing the whole host still requires restoring a canonical backup on another
host; Redis replication does not change that recovery boundary.
