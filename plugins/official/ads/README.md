# Monetization policy and slots

The content kernel knows only named slots and structured disclosures. Network
providers, affiliate transforms, direct sponsors, and aggregate measurement
are separate adapters. No provider means an empty slot with no markup or
network request.

Consent is a default-deny resource gate: scripts, pixels, tracking URLs,
iframes, preconnects, and storage remain blocked until every declared purpose
is authorized. See `docs/ai2ai/AD-INTEGRATION.md` and
`docs/legal/EU-CONSENT.md` before adding a provider.

The static contract is the deliberately narrow
[`article_footer` house/direct-sponsor slice](../../../docs/monetization/FIRST-PARTY-STATIC-ADS.md).
It accepts plain text, an optional user-initiated HTTPS link, and an optional
same-origin content-addressed image. It is not an ad-network adapter and cannot
carry HTML, scripts, pixels, personalization, identifiers, measurement, or
storage instructions.

## Kakao AdFit adapter

The optional [`kakao.adfit`](../../../providers/kakao-adfit.yaml) adapter has a
fixed public surface:

- `site_top` and `site_bottom` are the only AdFit render slots;
- desktop units are exactly 728 × 90 and mobile units are exactly 320 × 100;
- each size and position has its own operator-issued `DAN-…` unit ID;
- ads are eligible only on public reader pages, never login, onboarding,
  Studio, administrator, API, or error views; and
- refusal, an unknown choice, an invalid configuration, or provider failure
  produces no AdFit element or request.

The adapter first obtains a first-party choice. It does not preconnect, fetch,
insert an iframe, or load any Kakao/Daum resource before a grant. After a grant,
the reader document mounts the provider-generated `kakao_ad_area` element and
loads only the explicit official SDK URL
`https://t1.kakaocdn.net/kas/static/ba.min.js`. Withdrawal removes mounted
units and blocks future provider requests.

The direct document mount is intentional. Kakao's operating policy prohibits
calling AdFit from a publisher-created iframe, and its SDK guide says not to
modify the generated unit information or code. The Kakao-operated SDK currently
creates its own safe-frame resources and calls legacy `daum.net` and
`daumcdn.net` hosts. Those hosts are declared in the provider record and
`network.connect` permission; OSB does not author a Daum-based loader, rewrite
the SDK, or proxy its internals.

Ad-unit IDs are public browser identifiers, not administrator or API secrets.
They are accepted through local environment configuration to avoid publishing
a deployment's media setup, but a consenting reader can inspect them. This DLC
therefore has no `secret.use` permission.

The complete configuration, CSP inventory, consent invariant, placement
contract, verification sequence, and rollback procedure are in
[`KAKAO-ADFIT.md`](../../../docs/monetization/KAKAO-ADFIT.md). The adapter does
not create or modify `ads.txt`.
