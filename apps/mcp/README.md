# OpenSoverignBlog MCP adapter

`osb-mcp` is a small Model Context Protocol (MCP) stdio adapter over the
authoritative OpenSoverignBlog HTTP API. It deliberately contains no model,
prompt, macro engine, browser automation, or direct SQLite/Redis access.

It implements the MCP 2025-11-25 lifecycle and tool contract and exposes:

- `osb_content_list`
- `osb_content_read`
- `osb_content_create` (write mode)
- `osb_content_revise` (write mode)
- `osb_content_publish` (write mode)

Create and revise only produce private immutable revisions. Publishing remains
a separate tool so an MCP host can ask the human for confirmation. Content is
always complete portable Markdown; inert macro fences, if any, are just content
for an external AI/script to construct.

## Build and run

```sh
cargo build --release -p osb-mcp
./target/release/osb-mcp --base-url https://blog.example.com --mode read
```

Read mode is the default and exposes no mutating tools. To enable writes, pass
the credential through the environment, never a command-line argument:

```sh
export OSB_MCP_TOKEN="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')"
./target/release/osb-mcp \
  --base-url https://blog.example.com \
  --mode write
```

Set that same 32-128-character unpadded Base64url value as `OSB_MCP_TOKEN` in the
server environment and restart the server. Bootstrap does not generate this
optional automation credential; add it deliberately to the protected deployment
environment. The server stores only its SHA-256
digest and accepts it solely for content list/private-read/draft/revise/publish
routes. It never authorizes administrator auth, AI2AI proposals, assets, runner
operations, Studio settings, or member APIs. An active administrator module is
required, and delivery-only nodes reject the credential.

This is one global static content credential, not a per-client token issuer or a
general administrator credential. To rotate it, update the protected environment
of the server and every MCP process, then restart every application and MCP
replica. To revoke all MCP writes, remove it from every server replica and restart
them. A browser administrator access key, legacy owner token, Passkey, OAuth/OIDC
token, or browser session must never be copied into an AI process.

If the blog is mounted below a base path, include it in `--base-url`; for
example, `https://host.example/blog`. Redirects are disabled so credentials
cannot be forwarded to another origin. Upstream requests have a timeout,
responses and stdio frames are bounded, and tool calls are locally rate-limited.
Write mode rejects plain HTTP except for exact localhost or loopback development.

Example MCP client entry after the release build:

```json
{
  "mcpServers": {
    "open-soverign-blog": {
      "command": "/absolute/path/to/OpenSoverignBlog/target/release/osb-mcp",
      "args": [
        "--base-url",
        "https://blog.example.com",
        "--mode",
        "read"
      ]
    }
  }
}
```

Write-mode secrets belong in the MCP client's protected environment or secret
store and must not be committed with client configuration.

## Design boundary

The adapter calls only documented API routes. It never writes SQLite or Redis,
never resolves macros, never samples an LLM, and never publishes as a side
effect of drafting. An AI remains free to generate a one-off prompt or script,
then use these small tools to apply the result with revision-conflict checks.

Protocol references:

- <https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle>
- <https://modelcontextprotocol.io/specification/2025-11-25/basic/transports>
- <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>
