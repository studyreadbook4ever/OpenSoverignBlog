# Contributing

Contributions are welcome under the [Unlicense](UNLICENSE). By submitting a
contribution, you certify that you have the right to release it under that
license and that it does not copy code, CSS, assets, test fixtures, or
documentation from a project whose notice or license would be incompatible with
that claim.

Architecture changes should preserve these boundaries:

- domain crates do not depend on Axum, SQLite, a model provider, or a theme;
- external plugins do not query core tables;
- optional modules remain removable without corrupting published content;
- AI2AI and plugin contracts stay versioned and machine-readable;
- new network access and secret access require an explicit capability;
- renderer changes include malicious-input and publish-parity tests.

Run before opening a change:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
npm run check
npm run supply-chain:check
npm run build
```
