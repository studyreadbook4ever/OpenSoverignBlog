# AI2AI advertising integration contract

This document tells a software agent how to propose an advertising,
sponsorship, affiliate, or donation integration. It is not permission to install
one and it is not an EU compliance certificate.

## Read before changing code

1. `providers/provider.schema.json`
2. `schemas/plugin-manifest.v1.schema.json`
3. `schemas/consent-policy.v1.schema.json`
4. `schemas/ad-disclosure.v1.schema.json`
5. `docs/legal/EU-CONSENT.md`
6. `SECURITY.md`

## Stop conditions

Do not propose installation when any of these are unknown:

- the actual legal actor, controller/vendor role, and operator contact;
- every script, iframe, pixel, redirect, API, preconnect, and asset domain;
- cookies, browser storage, device access, identifiers, data categories, and
  retention;
- purposes, terminal-access basis, GDPR basis where applicable, recipients,
  profiling, and third-country transfers;
- independent-developer eligibility, KYC, region, traffic, and content rules;
- secret names, webhook verification, CSP changes, sandbox behavior, and a
  failure/uninstall path;
- visible advertising/sponsorship/affiliate disclosure.

Missing facts are not defaults. Ask the operator or leave the entry
`catalog-only`.

## Required proposal sequence

```text
read provider evidence
→ validate catalog schema
→ compare adapter network/secret capabilities with the catalog
→ generate a consent policy draft
→ ask the operator to select a jurisdiction profile and approve legal bases
→ generate human UI and machine disclosure from the same policy
→ run a clean-browser test before consent
→ fail when any undeclared DNS/network/storage activity appears
→ run grant, reject, reload, and withdrawal tests
→ produce a permission diff and rollback plan
```

The clean-browser baseline includes DNS-prefetch, preconnect, dynamic import,
tracking URLs, pixels, iframes, service workers, cookies, local/session storage,
IndexedDB, and browser-derived identifiers. Before a valid decision, every
non-essential item must remain at zero.

## Runtime boundary

The content kernel stores named `MonetizationSlot` and structured
`AdDisclosure` values only. It never stores provider script HTML in Markdown or
the author-intent layer. A policy engine resolves a slot after consent and
security checks. No provider or a failed provider produces an empty slot and
must not break the article.

Affiliate links are derived at render time from an allowlisted merchant rule;
the portable source URL remains unchanged. A diff and per-document opt-out are
required. Direct sponsorship and house ads should be supported without a
third-party runtime.

## Agent response format

An installation proposal should return:

```yaml
provider_entry: providers/<id>.yaml
adapter_manifest: plugins/<id>/plugin.toml
consent_policy: legal-profiles/<site>.yaml
permission_diff: []
network_before_consent: []
network_after_each_purpose: {}
storage_before_consent: []
disclosures: []
unanswered_questions: []
rollback_steps: []
verification_status: proposed | technically-verified
```

Never return `EU compliant`, `legally approved`, or a fabricated sovereignty
score. Technical verification proves only that the implementation matched its
declarations during the test.
