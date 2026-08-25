# Chatstronomy release process

## Before tagging

1. Update the package version in `Cargo.toml` and refresh `Cargo.lock`.
2. Run `cargo fmt --all --check`.
3. Run `cargo test --all-features` and `cargo test --no-default-features`.
4. Run `cargo clippy --all-targets --all-features -- -D warnings`.
5. If the Direct protocol changed, update its schema, compatibility fixtures,
   and protocol version before releasing.
6. Confirm the Windows signing environment and Azure signing secrets are
   available to the canonical repository.

Create and push a signed or annotated `vX.Y.Z` tag only after CI is green. The
release workflow publishes:

- `chatstronomy-linux-x64.tar.gz`
- `chatstronomy-linux-arm64.tar.gz`
- signed `chatstronomy-windows-x64.exe`
- `chatstronomy-windows-x64.zip`
- signed `chatstronomy-plugin-runtime-windows-x64.exe`
- `chatstronomy-plugin-runtime-windows-x64.zip`
- `chatstronomy-plugin-contracts-v1.zip`
- `chatstronomy-runtime-manifest.json`

The runtime manifest records the release identity, Direct protocol versions,
artifact names, sizes, and SHA-256 hashes after signing.

Every Linux archive, Windows ZIP, and plugin-contract archive includes both the
Apache-2.0 `LICENSE` and the `LiberationSans-LICENSE` notice for the embedded
chart font. The standalone signed Windows executables remain available for
existing plugin runtime locks and expose the same embedded notices through
`chatstronomy licenses`.

## Plugin follow-up

After the backend release succeeds, update `runtime.lock.json` in the
standalone `chatstronomy-nina-plugin` repository to the exact tag and manifest
checksum. Package and test the plugin there, sign its DLL, then publish its
registry entry. Plugin packaging must never fetch `latest` or build Rust.
