# Kakao AdFit web adapter

Status: core-maintained optional adapter, provider evidence and live loader
reviewed on **2026-07-24**.

This adapter adds two fixed banner positions to public OpenSoverignBlog reader
pages. It is a technical integration profile, not Kakao media approval, legal
advice, or a compliance certification. The operator remains responsible for
its AdFit account, registered media, current provider policy, visitor notice,
legal basis, content, placement, and production network trace.

## Fixed scope

| Position | Desktop unit | Mobile unit |
| --- | --- | --- |
| `site_top` | 728 × 90 | 320 × 100 |
| `site_bottom` | 728 × 90 | 320 × 100 |

`site_top` is after the site header and before reader content. `site_bottom` is
after reader content and before the site footer. There is no inline, sidebar,
popup, interstitial, Studio, login, onboarding, administrator, API, or error
placement. At most two AdFit banners can appear on one page.

The top and bottom spaces reserve the selected unit's exact dimensions. Do not
place navigation, forms, download controls, pagination, or other clickable
controls against them. Kakao's operating policy requires fixed space and
prohibits layouts that induce or cause accidental clicks.

## Operator configuration

Create and approve the media in AdFit, then create four banner units with the
sizes above. Kakao says media review needs at least one unit and generally takes
one or two business days, although it can take longer. Use the exact code and
unit information issued for the registered media.

Configure all four values together:

| Environment variable | Value |
| --- | --- |
| `OSB_KAKAO_ADFIT_PC_TOP_UNIT` | desktop top `DAN-…` unit ID |
| `OSB_KAKAO_ADFIT_PC_BOTTOM_UNIT` | desktop bottom `DAN-…` unit ID |
| `OSB_KAKAO_ADFIT_MOBILE_TOP_UNIT` | mobile top `DAN-…` unit ID |
| `OSB_KAKAO_ADFIT_MOBILE_BOTTOM_UNIT` | mobile bottom `DAN-…` unit ID |

The adapter fails closed when the set is partial, malformed, duplicated, or
enabled without the `ads` DLC and feature. An entirely absent set leaves AdFit
inactive.

A `DAN-…` value is sent to the browser as the provider's ad-unit identifier. It
is not an AdFit account password, administrator key, API credential, or
server-side secret. A local ignored environment file is still a convenient way
to avoid committing deployment-specific media identifiers. Never put an AdFit
account password, Kakao account credential, settlement information, or
administrator key in these fields.

## Default-deny load sequence

Before a visitor grants the declared advertising purposes, the page MUST NOT:

- create an AdFit element or provider iframe;
- fetch, import, preload, prefetch, or preconnect the SDK;
- request an ad, impression, measurement pixel, creative, or provider asset;
- write provider storage or place the unit IDs in provider markup; or
- infer a grant from scrolling, closing, continued use, or another feature.

The first layer offers equally available accept, reject, and detail controls.
The first-party choice remembers only grant or refusal, without a unique
visitor ID. Rejection persists as reliably as acceptance, and a persistent
privacy-settings control lets the reader withdraw or grant later.

Only after a grant may the public reader document mount the exact
provider-generated `kakao_ad_area` attributes for the selected position and
viewport and request:

```text
https://t1.kakaocdn.net/kas/static/ba.min.js
```

That is the only SDK loader authored by the adapter. It uses an explicit
Kakao-hosted HTTPS URL, not a Daum-based loader and not a scheme-relative URL.
The provider guide warns that changing unit information or SDK code can cause
invalid or failed requests, so OSB must not patch, proxy, self-host, or rewrite
the loader.

The mount occurs directly in the public reader document. Kakao's operating
policy lists calls from publisher-created iframes among prohibited delivery
methods, so an isolation iframe is not a valid substitute. Kakao's own SDK can
internally create vendor frames; those are provider behavior and do not permit
OSB to wrap the integration in another iframe.

## Network and CSP contract

The live official loader reviewed on 2026-07-24 contained these resource hosts:

| CSP directive | Allowlisted provider origins |
| --- | --- |
| `script-src` | `https://t1.kakaocdn.net` |
| `connect-src` | `https://serv.ds.kakao.com`, `https://display.ad.daum.net`, `https://kaat.daum.net`, `https://aem-kakao-collector.onkakao.net` |
| `img-src` | `https://t1.kakaocdn.net`, `https://analytics.ad.daum.net`, `https://kaat.daum.net` |
| `frame-src` | `https://t1.kakaocdn.net`, `https://t1.daumcdn.net` |

The explicit loader remains Kakao-based. The legacy Daum origins above are
embedded in and operated through the current Kakao AdFit SDK for ad requests,
measurement, click/impression handling, or its internal safe frame. Removing
them can break an approved unit; rewriting them would alter provider code.

This inventory is a dated observation, not a promise that the mutable provider
SDK will never change. Advertiser click destinations and creative delivery can
also vary. A new origin remains blocked until it is documented, reviewed, and
approved; do not respond to a CSP report by adding a wildcard.

After consent the third-party script runs in the reader document, so CSP does
not isolate it from document context. Keep it completely absent from login,
onboarding, Studio, administrator, and other sensitive views. Do not place
credentials, draft content, or private account state in public reader markup.

## Data and purpose boundary

The adapter declares one optional provider gate containing
`ads.delivery`, `ads.measurement`, and `ads.personalization`. This conservative
set reflects that Kakao's terms require a publisher to disclose that Kakao
and/or the publisher may collect or use anonymous internet-use information,
including cookies, for ad quality, while the public web documentation does not
provide a complete configuration-specific cookie, retention, profiling, or
personalization inventory.

An operator must replace the generic detail text with its actual controller,
provider/recipient, retention, transfer, contact, and jurisdiction facts. See
[`EU-CONSENT.md`](../legal/EU-CONSENT.md). The OSB gate proves only that the
declared resources stayed blocked or were released according to the recorded
choice; it does not decide which law or legal basis applies.

## Verification

Do not click live ads or generate artificial impressions while testing.
Provider policy prohibits manual or automated inflation, including excessive
self-testing.

Use a clean browser profile and verify:

1. Before any choice, reject, reload, and withdrawal each produce zero requests
   to every host in the network table and no `kakao_ad_area` element.
2. Accepting on a public reader page mounts only top and bottom, selects
   728 × 90 for desktop or 320 × 100 for mobile, and fetches the exact Kakao
   loader only after the action.
3. `/login`, `/onboarding`, every `/studio/**` route, administrator/API paths,
   and error pages remain provider-network silent even after a prior grant.
4. The page reserves fixed space, stays usable without an ad or click, and
   keeps controls far enough away to avoid accidental clicks.
5. Every post-grant DNS request, redirect, cookie, storage operation, frame,
   image, script, and connection matches the provider record and the
   operator's reviewed consent policy.
6. Refusal and withdrawal remain available and do not guilt, punish, or
   repeatedly prompt the reader.

Re-run this trace whenever the provider code, unit configuration, purposes,
privacy terms, placement, routes, or CSP reports change.

## `ads.txt` and rollback

No `ads.txt` requirement was found in the official sources reviewed for this
adapter. OpenSoverignBlog therefore does not create, restore, or modify
`/ads.txt`. Re-check the AdFit dashboard and current official policy before an
operator chooses to add one; never fabricate a seller record.

To roll back, disable the `ads` feature/DLC and remove all four local unit-ID
values together. Restart with a validated configuration, confirm the
capabilities document no longer advertises AdFit, and run the pre-consent
network test again. Removing the adapter does not itself delete data already
held by Kakao; follow the applicable provider and privacy process for that.

## Official sources

- [Kakao AdFit product overview and media review flow](https://adfit.kakao.com/info)
- [Kakao AdFit Web guide](https://adfit.github.io/wiki/web-guide/)
- [Official AdFit Web SDK guide repository](https://github.com/adfit/adfit-web-sdk)
- [Kakao AdFit operating policy](https://adfit.kakao.com/web/html/use_kakao.html)
- [Kakao AdFit terms of service](https://adfit.kakao.com/web/html/stipulation_kakao.html)
- [Kakao Business privacy policy](https://business.kakao.com/policy/privacy/)
