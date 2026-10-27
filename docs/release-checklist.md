# 1.0.0 release validation

The manifest and lockfile are prepared for 1.0.0. Publishing, credentials, and
all Git actions remain developer-managed. No publication or registry-availability
check has been performed.

## Contract decisions

README.md documents the public API, JSON formats, limit policies, byte/depth
accounting, exit status, and output routing. Public structs retain public fields;
adding fields may affect exhaustive literals and needs semver review. Snapshots
support serialization, not deserialization or resuming a sampler. No additional
API changes were made during release preparation.

## Validation

- 29 isolated unit tests pass. The current implementation was also checked with
  the installed Rust 1.87.0 minimum toolchain; no downloads were needed.
- The public-library doctest passes without default features.
- Formatting and all-target/all-feature Clippy checks pass.
- Ten release-binary process checks pass on this Linux environment: bundled
  example and version, full expanded-JSON summary files, actual SIGINT during
  idle input, final-only SIGINT with pending data, broken stdout with/without a
  summary file, file-open and file-write failures, redirected colors/JSON, and
  automatic colors on a PTY.
- Offline locked `cargo package --allow-dirty` verification passes; all 29 unit
  tests and the library-only doctest also pass from the extracted crate.
- Release compilation emits a linker warning about an ignored deprecated
  optimization setting; compilation and every check still complete successfully.
- Tests use temporary files and local processes only. macOS/Windows behavior
  has not been exercised here.

Reproduce from this repository:

```sh
../../scripts/with-rust cargo test --offline --locked --lib --bins
../../scripts/with-rust cargo test --offline --locked --doc --no-default-features
../../scripts/with-rust cargo +1.87.0 test --offline --locked --lib --bins
../../scripts/with-rust cargo fmt --check
../../scripts/with-rust cargo clippy --offline --all-targets --all-features -- -D warnings
../../scripts/with-rust cargo build --offline --locked --release
python3 tools/check-release.py target/release/json-sampler
../../scripts/with-rust cargo package --offline --locked --allow-dirty
```

The package includes `example.ndjson` because a unit test embeds it at compile
time, as well as the process-check script under `tools/`. The `--allow-dirty`
flag permits verifying the current working tree without staging or committing.

The workspace benchmark `scripts/measure-json-sampler` remains optional and has
not been run end to end. No throughput or peak-memory claims have been made.

Before publishing, the developer should review the final diff, manage Git
state/history, and confirm crates.io name/version availability in their release
credential context. This validation did not contact external systems.
