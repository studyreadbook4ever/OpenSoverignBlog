# EU consent and advertising profile

Status: implementation guidance for OpenSoverignBlog, reviewed against the official sources listed below on **2026-07-18**.

This document is a conservative interoperability profile, not legal advice, a certification, or a guarantee of compliance. EU privacy and advertising rules depend on facts, roles, Member State law, and later guidance or judgments. An operator remains responsible for its deployment and should obtain qualified advice where appropriate.

## 1. What this profile covers

Use this profile when an OpenSoverignBlog deployment may:

- store information on, or read information from, a visitor's device;
- process personal data for analytics, advertising, personalisation, or measurement;
- embed third-party scripts, pixels, frames, SDKs, social widgets, fonts, media, or ads; or
- display sponsored, affiliate, native, or otherwise commercial content.

The GDPR can apply to an operator established in the EU and, in some circumstances, to a non-EU operator offering goods or services to people in the EU or monitoring their behaviour there. Device storage/access is also governed by national laws implementing Article 5(3) of the ePrivacy Directive. Configure a versioned jurisdiction profile; do not assume that this file replaces Member State analysis.

The safest portable default is privacy-preserving behaviour for every visitor. Geographic gating is an operator choice, but IP-derived location is fallible and is itself data processing.

## 2. Normative words and source status

In this document, **MUST**, **MUST NOT**, **SHOULD**, and **MAY** describe the OpenSoverignBlog profile, not a new statement of law.

Machine-readable legal sources use these status values:

| Status | Meaning |
| --- | --- |
| `binding-law` | An enacted EU legal act. Applicability still depends on scope and facts. |
| `national-transposition-required` | An EU directive whose operative rule is implemented in Member State law. |
| `court-interpretation` | A judgment interpreting EU law. |
| `edpb-guidance` | Supervisory-authority guidance; authoritative but not legislation. |
| `voluntary-best-practice` | A non-binding design recommendation. |
| `proposal-ongoing` | A legislative proposal that is not current binding law. |
| `standardisation-work-item` | Planned standardisation, not an adopted technical standard. |

An AI agent MUST preserve this distinction. In particular, it MUST NOT present a `proposal-ongoing`, voluntary pledge, private industry framework, or future standard as proof of legal compliance.

## 3. Runtime invariants

The consent module and every resource-loading adapter MUST enforce these rules:

1. **Default deny.** Before a valid choice, block every device-storage/access operation that is not either solely needed to transmit a communication or strictly necessary to provide a service explicitly requested by the visitor.
2. **Technology neutrality.** Apply the gate to more than cookies: local or session storage, IndexedDB, cache storage, tracking URLs and pixels, JavaScript instructions, iframes, SDKs, persistent identifiers, fingerprinting, and comparable client-side access can be in scope.
3. **Purpose and actor binding.** A grant authorises only the declared purpose revision, actors, operations, and resources. A new or materially changed purpose requires a new choice.
4. **No undeclared network escape.** A resource absent from the validated policy manifest MUST be blocked in EU-consent mode. Redirects and dynamically inserted resources are subject to the same check.
5. **Load after decision.** Consent-dependent code MUST NOT be fetched, executed, preconnected, or used to transmit data before the required grant. Merely hiding a widget is insufficient.
6. **Withdrawal takes effect.** On withdrawal, stop future processing covered only by consent, unload or disable adapters where technically possible, and request deletion or suppression from downstream providers as the applicable contract and law require. Withdrawal does not make earlier valid processing unlawful.
7. **No silent legal-basis swap.** A withdrawn or invalid consent MUST NOT be retrospectively relabelled as legitimate interests. Article 5(3) device access also cannot be bypassed merely by selecting a GDPR legal basis.
8. **Fail closed.** Invalid policy, unknown purpose, missing integrity data, or unavailable consent state means that optional processing remains blocked.

The two Article 5(3) exceptions are narrow and fact-specific. Every `strictly-necessary` resource MUST have a human-readable rationale and a test showing the requested feature would fail without it. Convenience, audience measurement, or monetisation alone is not enough.

If a deployment has no consent-dependent purpose or resource, do not show a performative consent banner. Keep the privacy notice and settings route available, and mount the banner only when there is a real optional choice to make.

## 4. Consent experience

### First layer

Before optional processing starts, the first layer MUST provide, in clear language:

- the relevant purposes and a concise description of the processing;
- the controller identity and an accessible route to the actor/provider list;
- an upfront statement when access is financed wholly or partly by advertising;
- `Accept all`, `Reject all`, and `Manage` controls that are simultaneously available and comparably discoverable;
- a link to detailed privacy, retention, transfer, profiling, and withdrawal information; and
- no preselected optional purpose, implied consent, or consent inferred from scrolling, swiping, closing, or continued browsing.

Visual design MUST NOT disguise rejection through low contrast, ambiguous wording, extra friction, or misleading hierarchy. Exact colour and geometry are contextual, so automated tests should check parity and accessibility without claiming that one palette is universally lawful.

A cookie wall that offers no genuine choice is prohibited by this profile. A deployment considering a paid or otherwise equivalent alternative needs its own legal assessment and a separate policy extension.

### Granularity and information

Optional purposes MUST be separately controllable unless the operator documents why they form one inseparable purpose. Consent is not bundled into unrelated terms. The detailed layer SHOULD expose:

- controller, representative, and DPO contact details where applicable;
- each purpose and applicable legal basis;
- actor roles, recipients, provider domains, and privacy URLs;
- data categories, device operations, retention period or criteria;
- third-country destinations and transfer safeguards;
- profiling or automated-decision information;
- data-subject rights, supervisory-authority complaint route, and withdrawal method; and
- cookie or identifier duration and whether third parties can access them.

Use short first-layer copy with progressive disclosure; do not omit required information merely because the detailed representation is machine-readable.

### Refusal and withdrawal

Rejection MUST last at least as reliably as acceptance. A generic refusal flag is preferred over a unique visitor identifier. A one-year refusal memory is a conservative voluntary design baseline, not a universal statutory period; a jurisdiction profile or user action can shorten it.

A persistent `Privacy settings` control MUST remain available from every public page. Withdrawing a grant MUST be no harder than giving it. Do not repeatedly ask after refusal unless the declared purpose or actors materially change, the stored choice expires under a documented policy, or the visitor opens settings. Keep any re-prompt interval configurable because national rules and future EU law may differ.

## 5. Evidence and data minimisation

The operator must be able to demonstrate a valid consent. A receipt SHOULD record only what is needed to prove the event:

- policy identifier, version, and integrity digest;
- purpose identifiers and purpose revisions;
- decision time, source, and active action;
- locale, presented UI version/digest, and available controls;
- actors bound to each decision; and
- withdrawal or supersession links.

Use [`consent-receipt.v1.schema.json`](../../schemas/consent-receipt.v1.schema.json) for this record. It deliberately prohibits declaring a raw IP address or full user-agent string as receipt evidence. If an operator needs additional security logs, keep them in a separately governed system instead of expanding every consent receipt. Evidence retention must be documented and limited; proof obligations do not justify indefinite behavioural tracking.

The policy itself uses [`consent-policy.v1.schema.json`](../../schemas/consent-policy.v1.schema.json). Hash a canonical serialisation, store the digest with the receipt, and archive the exact human-facing content version. To avoid a circular digest, the recorded canonicalisation method MUST identify the digest field it excludes (for example, RFC 8785/JCS excluding `/evidence/policy_integrity`). A receipt is evidence of an asserted interaction, not a compliance certificate.

## 6. Advertising disclosures

Every advertisement or commercial communication MUST be visibly identifiable as such, and the person or organisation on whose behalf it is shown MUST be identifiable. Paid editorial-style promotion and affiliate consideration require an adjacent or in-content disclosure that an ordinary reader can notice before acting on it.

Use [`ad-disclosure.v1.schema.json`](../../schemas/ad-disclosure.v1.schema.json) as the source for both the visible label and AI2AI metadata. It records:

- ad type and consideration;
- advertiser and, where different, payer;
- provider and resource domains;
- whether targeting is contextual, cohort-based, or personalised;
- main targeting parameters and user controls;
- device access, personal-data, profiling, and consent-purpose links; and
- the operator's DSA applicability assessment.

The Digital Services Act's Article 26 transparency duties apply when the service is an **online platform** within the Act, not automatically to every personal blog. Where applicable, each ad must be identifiable in real time, identify the beneficiary and payer where different, explain the main targeting parameters and how to change them, and avoid profiling with special-category data. The schema can carry these fields even when the operator records `not-applicable` or `not-assessed`; that value is not a legal conclusion by the project.

Embedding an ad, social plugin, or similar third-party component can make the site operator a joint controller for the collection and transmission stage even when the operator never sees the resulting data. Provider documentation is therefore input to, not a substitute for, the operator's own actor and purpose mapping.

## 7. Browser signals and private frameworks

A consent adapter MAY honour privacy-preferring browser signals as a denial or objection. This profile treats a software signal as affirmative consent only if the surrounding interaction independently proves that the choice was active, informed, specific, granular, and bound to the current actors and purposes. A generic signal MUST NOT silently grant every purpose.

Private frameworks such as IAB Europe's Transparency and Consent Framework can be implemented as optional adapters. They are not EU approval or a safe harbour. The Court of Justice has held that a TC String can be personal data when it can reasonably be linked to an identifier, and that a framework operator can be a joint controller for relevant stages. Keep framework strings out of the core domain model; translate them through a versioned adapter and retain only necessary evidence.

### Digital Omnibus status

The Commission's Digital Omnibus proposal, COM(2025) 837, proposes changes including centralised machine-readable browser choices, easier rejection, and limits on repeated requests. As of **2026-07-18**, EUR-Lex lists the procedure as **ongoing**. Those provisions are therefore `proposal-ongoing`, not current binding obligations.

The 2026 EU standardisation work programme separately includes work on machine-readable expression of consent choices. It is a `standardisation-work-item`, not an adopted interoperability standard. Implement signal support behind adapters and capability negotiation so that a later standard can be added without changing stored domain objects.

## 8. AI2AI integration contract

An AI agent installing an ad, analytics, embed, or consent plugin MUST:

1. read this profile and the three schemas;
2. enumerate every client-side operation, outbound domain, redirect, actor, purpose, retention rule, and transfer;
3. generate or update a consent policy and ad disclosure without inventing legal roles or provider facts;
4. validate all documents against their schemas, then separately verify unique IDs, resolved cross-references, actor/domain coverage, and resource-to-purpose consistency;
5. require explicit operator confirmation for legal basis, Article 5(3) exception, actor role, DSA assessment, and jurisdiction choices;
6. run a clean-browser network test for `no choice`, `reject all`, each granular grant, and withdrawal;
7. fail the installation if an optional request occurs before consent or an undeclared domain is contacted; and
8. report source statuses and unresolved assumptions in the installation output.

An AI agent MUST NOT label a deployment `GDPR compliant`, `EU approved`, or equivalent based only on schema validation. Validation proves document shape and selected safety invariants, not truth, legal sufficiency, or runtime behaviour.

Suggested plugin installation flow:

```text
provider manifest
  -> actor/purpose/resource inventory
  -> operator review of legal assertions
  -> schema validation
  -> consent-gated build
  -> clean-browser network tests
  -> signed policy digest + deploy
  -> periodic/provider-change review
```

## 9. Official sources

The links below are primary EU materials. Dates identify adoption, judgment, publication, or the version relied upon; always check for later amendments.

| Status | Source | Relevant point |
| --- | --- | --- |
| `binding-law` | [GDPR, Regulation (EU) 2016/679](https://eur-lex.europa.eu/eli/reg/2016/679/2016-05-04/eng) | Territorial scope; consent; transparency; objection; privacy by design/default. |
| `national-transposition-required` | [Directive 2009/136/EC amending ePrivacy Article 5(3)](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A32009L0136) | Prior consent for terminal storage/access and the two limited exceptions. |
| `court-interpretation` | [Planet49, C-673/17, 2019-10-01](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A62017CJ0673) | Active consent; no prechecked boxes; duration and third-party access information. |
| `court-interpretation` | [Fashion ID, C-40/17, 2019-07-29](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A62017CJ0040) | Possible joint controllership for collection/transmission through an embedded plugin. |
| `court-interpretation` | [IAB Europe, C-604/22, 2024-03-07](https://eur-lex.europa.eu/legal-content/EN/CASE/?uri=CELEX%3A62022CJ0604) | TC String as potentially personal data and framework-stage joint controllership. |
| `edpb-guidance` | [EDPB Guidelines 05/2020 on consent, version 1.1](https://www.edpb.europa.eu/system/files/documents/files/file1/edpb_guidelines_202005_consent_en.pdf) | Freely given, granular, informed consent; evidence; renewal; withdrawal. |
| `edpb-guidance` | [EDPB Cookie Banner Taskforce report, 2023-01-17](https://www.edpb.europa.eu/system/files/2023-01/edpb_20230118_report_cookie_banner_taskforce_en.pdf) | Common positions on rejection, preselection, deceptive links, exemptions, and withdrawal. |
| `edpb-guidance` | [EDPB Guidelines 2/2023 on the technical scope of ePrivacy Article 5(3), version 2.0](https://www.edpb.europa.eu/system/files/2024-10/edpb_guidelines_202302_technical_scope_art_53_eprivacydirective_v2_en_0.pdf) | Technology-neutral treatment beyond conventional cookies. |
| `voluntary-best-practice` | [EDPB opinion on draft Cookie Pledge principles, 2023-12-13](https://commission.europa.eu/document/download/cad5989c-4b27-44ad-b29c-b8cc087f4dae_en?filename=Annex+III+-+opinion+of+the+EDPB+on+draft+pledging+principles.pdf) | First-layer rejection, ad-funded disclosure, refusal memory, and software signals. |
| `voluntary-best-practice` | [European Commission Cookies Pledge project summary, 2024-11-08](https://commission.europa.eu/publications/cookies-pledge-summary-project_en) | Voluntary work on less repetitive, clearer cookie choices. |
| `binding-law` | [E-Commerce Directive 2000/31/EC, Article 6](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A02000L0031-20240217) | Identifiability of commercial communications and their beneficiary. |
| `binding-law` | [Unfair Commercial Practices Directive 2005/29/EC, Annex I(11)](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A02005L0029-20220528) | Clear disclosure of paid editorial promotion. |
| `binding-law` | [Digital Services Act, Regulation (EU) 2022/2065, Article 26](https://eur-lex.europa.eu/eli/reg/2022/2065/oj?locale=en) | Online-platform ad transparency and targeting restrictions. |
| `proposal-ongoing` | [Digital Omnibus proposal COM(2025) 837](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A52025PC0837) and [legislative procedure status](https://eur-lex.europa.eu/legal-content/EN/HIS/?uri=COM%3A2025%3A837%3AFIN) | Proposed consent-signal and request-frequency reforms; ongoing as of the review date. |
| `standardisation-work-item` | [2026 EU standardisation work programme, C/2026/1695](https://eur-lex.europa.eu/legal-content/EN/TXT/?uri=CELEX%3A52026XC01695) | Planned work on machine-readable expression of consent choices. |

## 10. Review triggers

Review the profile and regenerate affected policy versions when any of these changes:

- purpose, provider, actor role, domain, identifier, retention, or transfer;
- banner wording, layout, signal semantics, or withdrawal path;
- browser or framework adapter version;
- applicable Member State or audience;
- official guidance, judgment, enacted Digital Omnibus text, or interoperability standard; or
- a network test detects an undeclared or pre-consent request.

Record the review date and source statuses in the policy. Never mutate an already referenced policy version; publish a new version and link replacement receipts through `supersedes_receipt_ids`.
