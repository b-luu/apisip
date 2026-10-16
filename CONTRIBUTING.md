# Contributing to apisip

Thanks for helping. This file covers building, testing and sending changes.
For what apisip does and how to use it, read the [README](README.md). For how
it works inside (detection, the commit protocol, the output layout), read
[docs/design.md](docs/design.md).

## Set up

You need a stable Rust toolchain (1.89 or later) and, for the view tests,
the `duckdb` CLI on your `PATH`. Nix users can run `nix develop` instead, which
provides cargo, clippy, rustfmt, rust-analyzer and duckdb.

## Build and test

Run these before every pull request. CI runs the same three commands:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The default suite runs offline. It contains:

- unit tests next to the code, in `src/`;
- end-to-end tests in `tests/e2e.rs`, which run the real binary against the
  bundled mock API;
- `init` tests in `tests/init.rs`, one of which drives the interactive prompts
  through a pseudo-terminal;
- crash tests in `tests/run.rs` and `tests/e2e.rs`, which use fail points that
  exist in debug builds only (`APISIP_FAIL_POINT`).

Tests that read the generated views skip themselves when `duckdb` is missing.
Set `APISIP_REQUIRE_DUCKDB=1` to make them fail instead.

Optional suites:

- **Live public APIs** (Petstore, the GitHub spec), which need the network:

  ```sh
  cargo test --test e2e -- --ignored
  ```

- **S3**, which needs a reachable S3-compatible bucket (SeaweedFS, MinIO,
  Garage...):

  ```sh
  APISIP_TEST_S3=1 APISIP_TEST_S3_URL=s3://<bucket>/<prefix> \
    AWS_ENDPOINT_URL=http://127.0.0.1:9000 AWS_ACCESS_KEY_ID=... \
    AWS_SECRET_ACCESS_KEY=... AWS_REGION=us-east-1 cargo test --test e2e s3
  ```

- **Nix**: `nix flake check` builds the package and runs the test suite in the
  sandbox.

## Try a change by hand

The mock API in `examples/mock-api/` covers each auth scheme, the pagination
styles, the three incremental modes, rate-limit headers and spec drift:

```sh
cargo run --example mock-api            # serves http://127.0.0.1:8080
mkdir /tmp/try && cd /tmp/try
cargo run --manifest-path <repo>/Cargo.toml -- init http://127.0.0.1:8080/openapi.json
```

The README quickstart walks through the rest.

## Tests for bug fixes

A bug fix comes with a test that fails without the fix. Before you send it,
revert the fix, run the test and see it fail, then restore the fix and see it
pass. A test that passes either way does not protect anything.

## Fixtures

Test specs are written by hand for this repo, or come from public examples
with their license noted in [tests/fixtures/README.md](tests/fixtures/README.md).
Live specs are fetched at test time and never committed.

## Commits and pull requests

- Commit messages follow `type(scope): description`, with `type` one of
  `feat`, `fix`, `docs`, `chore`, `test` or `refactor`.
- One concern per commit.
- Keep the pull request description to one screen: what changes, why, and how
  you checked it.
- Name things by what they are. Do not cite internal ticket or plan IDs.
- Never put real credentials in code, tests, fixtures or logs.

## Security

Report a vulnerability, such as a way to make apisip print or send a
credential, privately through GitHub's security advisory form, not in a
public issue.

## License

Unless you say otherwise, any contribution you submit is dual-licensed under
the [Apache License, Version 2.0](LICENSE-APACHE) and the
[MIT license](LICENSE-MIT), with no additional terms or conditions.
