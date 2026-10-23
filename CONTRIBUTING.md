# Contributing to apisip

Thanks for helping. This file covers building, testing and sending changes.
For what apisip does and how to use it, read the [README](README.md). For how
it works inside (detection, the commit protocol, the output layout), read
[docs/design.md](docs/design.md).

## Set up

You need a Rust toolchain and, for the view tests, the `duckdb` CLI on your
`PATH`. The crate builds on Rust 1.89 or later. CI pins the toolchain it runs
the gates on in `rust-toolchain.toml`, and rustup picks that file up
automatically. Nix users can run `nix develop` instead, which provides cargo,
clippy, rustfmt, rust-analyzer and duckdb.

## Build and test

Run these before every pull request. CI runs the same three commands:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs these jobs on every pull request:

- `gates`: the three commands above, on the pinned toolchain, with
  `APISIP_REQUIRE_DUCKDB=1`.
- `msrv`: `cargo check --all-targets` on Rust 1.89.
- `s3`: the S3 suite against a SeaweedFS container.
- `machete`: `cargo machete` fails on an unused dependency.
- `mutants`: `cargo mutants --in-diff` changes the lines your pull request
  touched and fails if the tests still pass. It checks that your tests notice a
  broken version of your change.
- `coverage`: `cargo llvm-cov` reports line coverage in the job summary. It has
  no threshold.

Two more run outside the pull request flow. `Nix` runs `nix flake check` on
pushes to main, weekly, and on pull requests that touch the flake or the
dependencies. `Toolchain canary` runs clippy and the tests on the latest stable
Rust every week and opens an issue when they fail.

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
  Garage...). CI uses SeaweedFS. To run the same setup, put an identity in
  `s3.json`:

  ```json
  {"identities":[{"name":"test","credentials":[{"accessKey":"apisiptest","secretKey":"apisiptest-secret"}],"actions":["Admin","Read","Write","List","Tagging"]}]}
  ```

  Start the server, create the bucket, then run the test:

  ```sh
  docker run -d -p 8333:8333 -v "$PWD/s3.json:/etc/s3.json:ro" \
    chrislusf/seaweedfs:4.48 server -dir=/data -s3 -s3.config=/etc/s3.json
  AWS_ACCESS_KEY_ID=apisiptest AWS_SECRET_ACCESS_KEY=apisiptest-secret \
    aws --endpoint-url http://127.0.0.1:8333 --region us-east-1 s3 mb s3://apisip-test
  APISIP_TEST_S3=1 APISIP_TEST_S3_URL=s3://apisip-test/e2e \
    AWS_ENDPOINT_URL=http://127.0.0.1:8333 AWS_ACCESS_KEY_ID=apisiptest \
    AWS_SECRET_ACCESS_KEY=apisiptest-secret AWS_REGION=us-east-1 \
    cargo test --test e2e s3
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

The README quickstart walks through the rest. The output lands in `data/`
inside that folder; `apisip inspect` shows what it holds without calling the
API.

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

## Releases

Maintainers only. Releases follow the conventional commits on `main`.

- On each push to `main`, release-plz opens or updates a release pull request
  that bumps the version and writes `CHANGELOG.md`. A `fix` makes a patch
  release and a `feat` a minor one; while the version is 0.x, a breaking change
  makes a minor release.
- Pull requests opened with the default `GITHUB_TOKEN` do not start CI. Close
  and reopen the release pull request to run the required checks on it. This is
  the chosen workaround; a GitHub App token would remove the step.
- Merging the release pull request publishes the crate to crates.io and pushes
  the `v<version>` tag. A tag pushed with `GITHUB_TOKEN` starts no workflow, so
  the release-plz workflow then starts the dist `release.yml` workflow on that
  tag by hand. It builds the archives and the shell installer and creates the
  GitHub release.
- `release.yml` is generated. Change `dist-workspace.toml` and run
  `dist generate`; do not edit the workflow. Every action in it is pinned by
  commit SHA through `github-action-commits`, so a dist upgrade needs those
  SHAs updated too.
- The first publish (0.1.0) is run by hand with `cargo publish`, because
  crates.io trusted publishing needs the crate to exist. After that, configure
  trusted publishing for this repository and the `release-plz.yml` workflow,
  and delete the `CARGO_REGISTRY_TOKEN` secret if there is one.

## Security

Report a vulnerability, such as a way to make apisip print or send a
credential, privately through GitHub's security advisory form, not in a
public issue.

## License

Unless you say otherwise, any contribution you submit is dual-licensed under
the [Apache License, Version 2.0](LICENSE-APACHE) and the
[MIT license](LICENSE-MIT), with no additional terms or conditions.
