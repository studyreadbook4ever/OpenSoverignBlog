# Isolated code runner client

This module is a broker client, not an evaluator. The Rust publishing process
never calls a compiler, shell, container engine, or language runtime. A runner
must be separately isolated with network off by default, read-only roots,
ephemeral writable space, PID/CPU/memory/time/output quotas, and no access to
the CMS database, blob directory, secrets, or Docker socket.

When absent, code remains readable and no run button is emitted. See
`docs/security/CODE-RUNNER.md`.

The reference composition root supports an authenticated remote broker client.
It validates an operator allowlist of code-fence aliases, profile IDs, and
immutable profile digests, checks broker readiness, disables redirects, bounds
timeouts and responses, and forces V1 job networking off. The capability is
advertised as active only after an exact readiness match. Configuration is in
`docs/operations/CONFIGURATION.md`.

The browser renders stdout and stderr as text. It never injects runner output
as HTML. Console profiles such as Rust and Kotlin can use the owner-only flow.
A Flutter/web preview needs a separately hosted sandboxed origin and is not
silently treated as a console job.
