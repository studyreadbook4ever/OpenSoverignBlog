# Static first-party ads

This is the smallest operationally safe monetization contract in
OpenSoverignBlog. It supports a house ad or a directly arranged sponsor in the
typed `article_footer` slot. It does not install an ad network.

The source contract is
[`first-party-ad.v1.schema.json`](../../schemas/first-party-ad.v1.schema.json).
The Rust implementation is in
[`features/monetization-policy`](../../features/monetization-policy/). An input
must pass both JSON Schema validation and Rust semantic authorization before it
can produce a render plan or HTML.

## Closed capability set

A v1 static ad contains only:

- the exact visible label `Advertisement` for a house ad or `Sponsored` for a
  direct sponsor;
- a stable ad ID, sponsor name, and plain-text body;
- an optional user-initiated, credential-free HTTPS destination; and
- an optional image already served by this site at
  `/media/<lowercase-sha256>`.

It cannot carry provider HTML, script, iframe, pixel, arbitrary image URL,
preconnect, identifier, personalization rule, measurement instruction, cookie,
or browser-storage operation. Every prohibited capability is an explicit
required `false` in the wire contract. Unknown properties are rejected. The
host must not reinterpret plain text as markup.

The included deterministic renderer escapes all text and URL attributes. The
outbound link uses `rel="nofollow sponsored noopener noreferrer"` and
`referrerpolicy="no-referrer"`. Navigation is possible only after the reader
clicks; the sponsor destination can still apply its own privacy rules and must
be reviewed by the operator.

## Relationship to consent

`authorize_without_consent()` means only that this object asks the renderer for
no optional storage, terminal access, personal data, identifier, or passive
third-party request. It is not a legal certification. If an adapter adds any
such operation, the static authorization fails closed and the operation must be
declared and authorized through `ResourceGate` and the active consent policy.

Do not display a performative cookie banner solely because this static contract
exists. Conversely, do not use this contract to bypass a banner or policy
required by another active feature. See
[`EU-CONSENT.md`](../legal/EU-CONSENT.md).

## Example

```json
{
  "schema_version": "1.0",
  "ad_id": "sponsor:example-2026",
  "kind": "direct_sponsor",
  "slot": "article_footer",
  "disclosure": "Sponsored",
  "sponsor_name": "Example Sponsor",
  "body_text": "This article is supported by Example Sponsor.",
  "click_url": "https://sponsor.example/product",
  "image": {
    "media_path": "/media/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "alt_text": "Example Sponsor"
  },
  "delivery": {
    "third_party_fetches": false,
    "scripts": false,
    "tracking_pixels": false,
    "raw_html": false,
    "personalization": false,
    "browser_storage": false,
    "identifiers": false,
    "measurement": false
  }
}
```

The runtime integration still needs to select an approved record for a slot,
call the authorization method, and place the returned plan or escaped markup.
An invalid record resolves to an empty slot; it must not fall back to raw HTML.
