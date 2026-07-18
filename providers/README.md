# Monetization provider catalog

This catalog is evidence, not endorsement and not an installation registry.
An entry can describe a provider without shipping executable code. Runtime
status is explicit:

```text
catalog-only → community-adapter → verified-adapter → core-maintained
```

Provider eligibility, regions, traffic requirements, payout terms, APIs,
scripts, cookies, and policies change frequently. Every claim needs an official
source and `lastVerified`; stale entries stay visible but must be marked stale.
Do not turn missing information into a guess or a sovereignty score.

The seed catalog contains eight real providers verified against provider-owned
documentation on 2026-07-18, plus one fictional first-party sponsorship
template. All eight real entries are `catalog-only`: the repository ships no
provider adapter, credentials, endorsement, or promise that an operator is
eligible.

The machine index is [`index.json`](index.json), validated by
[`index.schema.json`](index.schema.json). Provider records conform to
[`provider.schema.json`](provider.schema.json). `supportedRegions` is a compact
human-facing summary rather than an ISO allowlist because providers variously
publish applicant, payout, marketplace, or demand regions. Consumers must read
the cited source and `evidenceGaps` before proposing an integration.

## Evidence rules

- Use only provider-owned documentation, current terms, policy, API, and help
  pages. A blog post, directory, search snippet, or community SDK is not enough.
- Represent missing facts explicitly in `evidenceGaps`. Use `unknown` for
  eligibility, storage, or personalization when the schema permits it. An empty
  `requiredDomains` value is never sufficient evidence that a provider makes no
  network requests.
- Record the integration boundary. A static outbound link can have no
  pre-navigation provider storage while the destination service still has its
  own cookies and data processing.
- Keep terms-derived assets and product data under the provider's license. The
  project Unlicense does not relicense provider code, marks, creative, API data,
  or contract-controlled content.
- Re-check evidence before use. `lastVerified` is a historical observation, not
  a freshness guarantee.

An adapter manifest must request the same domains and secrets declared by its
catalog entry. CI rejects undeclared network access. Catalog data and adapter
code have independent licenses and provenance.

## Community expansion and federation

Breadth is evidence-gated on purpose. A list of hundreds of names without
current first-party evidence would be actively unsafe for AI2AI installation:
it could invent eligibility, omit tracking domains, or silently accept obsolete
terms. Grow the catalog through small, reviewable provider pull requests:

1. copy the fictional template and set `adapterStatus: catalog-only`;
2. cite provider-owned pages for every factual claim and add unresolved facts to
   `evidenceGaps`;
3. add the record to `index.json` and run `npm run validate:contracts`;
4. obtain independent review before changing status or publishing adapter code;
5. periodically re-verify or mark the record stale without deleting its audit
   history.

External communities can maintain their own signed index using the same index
and provider schemas. A future federation client should pin an index URL and
digest, retain source provenance, reject duplicate provider IDs, and keep remote
entries `catalog-only` until the local operator approves them. This repository
does not yet fetch remote indexes or define a trust registry.

Start with [AD-INTEGRATION.md](../docs/ai2ai/AD-INTEGRATION.md). Copy the
fictional direct-sponsor example, replace every value with verified facts, and
run `npm run validate:contracts`.
