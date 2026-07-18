# Third-party notices

OpenSoverignBlog's original project files are dedicated under the
[Unlicense](LICENSE). That dedication applies only to material the project has
the right to dedicate. It does not replace, weaken, or relicense third-party
code, generated bundles, fonts, operating-system packages, or other assets.

The server binaries statically include Rust dependencies, and the browser
bundle includes JavaScript libraries and KaTeX assets. Those components retain
their own licenses. The following files are distributed with release images:

- `LICENSE` — the Unlicense for original OpenSoverignBlog material;
- `THIRD_PARTY_NOTICES.md` — license choices, provenance boundaries, and
  regeneration instructions;
- `THIRD_PARTY_LICENSES.txt` — license and notice files collected from the
  locked application dependency packages;
- `docs/legal/dependency-inventory.json` — deterministic Cargo and npm package
  inventory, including versions, sources, integrity data, and license
  expressions;
- `docs/legal/application-sbom.cdx.json` — deterministic CycloneDX 1.5 SBOM for
  locked application dependencies.

The generated inventory is intentionally conservative: it includes the whole
resolved Cargo graph and every third-party entry in `package-lock.json`, even
when a build target or tree-shaking step does not place a component in the
final executable or browser bundle.

## MPL-2.0 boundary

The locked Rust graph currently contains these dependencies available only
under MPL-2.0:

- `cssparser@0.37.0`, obtained through `ammonia`;
- `dtoa-short@0.3.5`, obtained through `cssparser`.

They are consumed as unmodified dependency source files and remain covered by
MPL-2.0 at the file level. Their presence does not change the license of
separate, original OpenSoverignBlog files. If a distributor modifies an
MPL-covered file and distributes the result, the MPL requirements continue to
apply to that covered file. Executable-form distributors must not restrict the
recipient's rights in the corresponding MPL-covered source and must tell the
recipient how to obtain it.

The exact source used for these versions is available from the crates.io
packages recorded by checksum in `Cargo.lock`:

- <https://crates.io/crates/cssparser/0.37.0>
- <https://crates.io/crates/dtoa-short/0.3.5>

The complete MPL-2.0 text is included in `THIRD_PARTY_LICENSES.txt` and is also
published at <https://www.mozilla.org/MPL/2.0/>.

DOMPurify is offered under the expression `(MPL-2.0 OR Apache-2.0)`. This
distribution elects the Apache-2.0 option for `dompurify@3.4.12`; its packaged
license text is included in `THIRD_PARTY_LICENSES.txt`.

## Other application dependencies

MIT, Apache-2.0, BSD, ISC, Unicode, Zlib, CC-BY-4.0, and other approved
expressions in the generated inventory remain the terms of their respective
components. In particular, the minified browser output does not turn React,
DOMPurify, KaTeX, or their assets into Unlicense material. Applicable copyright
and permission texts supplied by installed package archives are preserved in
`THIRD_PARTY_LICENSES.txt`. The bundle explicitly lists lockfile components
whose package archive did not carry a standalone notice file; their declared
license, integrity, and source remain recorded in the JSON artifacts.

The certificate dataset in `webpki-roots@1.0.8` is declared under
CDLA-Permissive-2.0. It remains third-party data; its packaged attribution and
license text are included in `THIRD_PARTY_LICENSES.txt`.

`caniuse-lite` is a CC-BY-4.0 build dependency recorded in the npm lockfile. It
is included in the inventory even though its database is not copied wholesale
into the served browser bundle. Its packaged attribution/license material is
preserved by the generated license-text bundle when installed on the release
build platform.

## Container boundary

The checked-in CycloneDX file covers application dependencies, not the Debian,
Node, or Rust builder image contents. The final runtime image also contains
Debian and explicitly installed OS packages; their package-specific notices
remain in `/usr/share/doc`. A release process claiming a complete *container*
SBOM must additionally scan the final immutable image digest and publish that
result beside the application SBOM. This repository does not represent the
application SBOM as a full base-image inventory.

## Reproducing the checked artifacts

Use the pinned lockfiles and install the npm dependency tree first:

```sh
npm ci
npm run supply-chain:generate
```

The generator uses only Node.js standard-library modules plus
`cargo metadata --locked`; it installs no license or SBOM generator. Generated
JSON deliberately omits timestamps and machine-specific paths. The license
text bundle is generated for the release build platform, which is Linux in CI
and in the Dockerfile.

CI runs the non-writing verification mode:

```sh
npm run supply-chain:check
```

That command fails if either lockfile and a checked artifact disagree, if an
npm license expression has not been reviewed, if an application package lacks
license metadata, or if an MPL-only Cargo dependency is missing from this
notice. Dependency updates must regenerate and review all three artifacts.

This notice records the project's distribution policy; it is not legal advice.
