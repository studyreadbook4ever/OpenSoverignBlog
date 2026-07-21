# Offline Markdown import

`osb local import` migrates a set of Markdown posts into the primary site
without enabling OAuth, member auth, or a remote administrator endpoint. It
preserves the post creation time, imported author metadata, primary category,
canonical slug, and historical path aliases.

Stop the writable server before using the maintenance container. Mount the
manifest directory read-only, run a dry run, then run the same command without
`--dry-run`:

```sh
docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  stop blog

docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  --profile maintenance run --rm -T \
  -v /srv/osb/import:/import:ro \
  osb-local local import \
  --manifest /import/import-manifest.json --dry-run --json

docker compose -p <compose-project> \
  --env-file /srv/osb/my-blog/.env \
  -f /path/to/OpenSoverignBlog/compose.yaml \
  --profile maintenance run --rm -T \
  -v /srv/osb/import:/import:ro \
  osb-local local import \
  --manifest /import/import-manifest.json --json
```

Restart the server with the exact command recorded by bootstrap after the
successful import. This reopens canonical SQLite state and rotates optional
Redis derivatives.

## Manifest schema

The importer accepts `open-soverign-blog-offline-import/1` JSON. Unknown fields
are rejected so a converter typo cannot silently discard migration metadata.
Every `markdownPath` is relative to the manifest, must stay below that
directory, and must resolve to a regular non-symlink UTF-8 file no larger than
10 MiB. The optional `contentSha256` is either 64 hexadecimal characters or
`sha256:` followed by that digest.

```json
{
  "schemaVersion": "open-soverign-blog-offline-import/1",
  "source": "eff0rtchung-static-v1",
  "ownerDisplayName": "me",
  "defaultAuthor": {
    "id": "legacy:me",
    "displayName": "me"
  },
  "categories": [
    {
      "slug": "yangja",
      "title": "Yangja",
      "description": "양자 컴퓨팅 학습 문서"
    }
  ],
  "posts": [
    {
      "sourceId": "yangja:grover",
      "title": "Grover",
      "slug": "grover",
      "markdownPath": "content/yangja/grover.md",
      "contentSha256": "7e458e45382d0b8df46aa66160fcc6b32e68b18d56f320bb5b32a62efe09bf63",
      "createdAt": "2026-06-18T15:57:00+09:00",
      "primaryCategory": "yangja",
      "humanReviewed": true,
      "legacyPaths": [
        {
          "path": "topics/algorithms/grover.html",
          "createdAt": "2026-06-18T15:57:00+09:00"
        }
      ]
    }
  ]
}
```

`ownerDisplayName` updates the primary site's public owner name in the same
transaction. Each post may override `defaultAuthor` with its own `author`;
that identity is retained on the immutable revision. Imported public
authorship uses `source` as its portable generator/provenance label.

`primaryCategory` must name an active category. A category declared in the
manifest is created if missing, or reused only when its title and description
match exactly. Canonical post paths become `/<primaryCategory>/<slug>`.

`legacyPaths` contain decoded, root-relative paths without a leading slash.
They may have up to 32 safe segments, so paths such as
`topics/algorithms/grover.html` work. Their `createdAt` defaults to the post
creation time and remains visible in exports. Paths that overlap application
routes are rejected. This includes fixed assets such as `/favicon.svg` and the
first segment of the effective `server.article_base_path`; for example,
`writing/articles` reserves the complete `/writing/...` subtree from aliases
and imported primary categories. "Effective" uses the same precedence as the
server: a non-empty `OSB_ARTICLE_BASE_PATH` overrides TOML. The `osb-local`
Compose service forwards that value, so always use the deployment's recorded
`--env-file`; maintenance must evaluate the same route namespace as the public
server.

The maintenance write boundary follows the same rule for `OSB_INTENT` and
`OSB_DELIVERY_ONLY`. Environment values override TOML only when non-empty,
invalid booleans or intents fail closed, and `delivery` must agree with
`delivery_only=true`. A consistent delivery configuration is still read-only,
and `osb local` refuses to open SQLite for writes.

## Atomicity and redirects

The complete batch, including the owner name and new categories, commits or
rolls back together. The key `(primary site, source, sourceId)` identifies one
immutable import. Re-running an identical manifest reports the post as
`unchanged`; changing its Markdown, timestamp, author, category, or aliases is
a conflict that leaves the database untouched.

Idempotency is per post rather than a frozen whole-manifest snapshot. A later
batch may reconcile `ownerDisplayName`, reuse exact existing category
declarations, and append new categories or new `sourceId` posts. Every
previously imported `sourceId` included again must still match its original
post exactly; use the normal revision workflow instead of changing an import
record.

The application resolves stored aliases on the apex host and responds with a
`301 Moved Permanently` redirect to the absolute canonical category URL.
Hostname migration is deliberately outside SQLite: keep gateway or DNS-edge
redirects from old hosts such as `yangja.example.com` to the apex host,
preserving the request path. Collection aliases that point to a category
landing page also belong at that gateway because route aliases are
document-scoped.
