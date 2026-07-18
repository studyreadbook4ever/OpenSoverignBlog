# Monetization policy and slots

The content kernel knows only named slots and structured disclosures. Network
providers, affiliate transforms, direct sponsors, and aggregate measurement
are separate adapters. No provider means an empty slot with no markup or
network request.

Consent is a default-deny resource gate: scripts, pixels, tracking URLs,
iframes, preconnects, and storage remain blocked until every declared purpose
is authorized. See `docs/ai2ai/AD-INTEGRATION.md` and
`docs/legal/EU-CONSENT.md` before adding a provider.

The first implemented contract is the deliberately narrow
[`article_footer` house/direct-sponsor slice](../../../docs/monetization/FIRST-PARTY-STATIC-ADS.md).
It accepts plain text, an optional user-initiated HTTPS link, and an optional
same-origin content-addressed image. It is not an ad-network adapter and cannot
carry HTML, scripts, pixels, personalization, identifiers, measurement, or
storage instructions.
