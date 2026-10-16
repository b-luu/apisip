# Notes for contributors and coding agents

apisip is a single Rust binary: OpenAPI spec in, JSONL or Parquet files out.
`docs/design.md` describes the behaviour; read it before changing detection,
the output layout or the commit protocol.

## Build and test

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Nix users can get the toolchain with `nix develop` and run `nix flake check`.
Tests that need the network or docker are `#[ignore]`d or gated behind an
environment variable (`APISIP_TEST_S3=1`), so the default suite runs offline.

## Rules

- Never reference private, client or employer projects in code, comments,
  docs, fixtures, commit messages or issues. Public open-source projects,
  formats and warehouses are fine to name.
- Test fixtures are written by hand for this repo, or come from public examples
  with their license noted in `tests/fixtures/README.md`.
- Never cite plan or work-item IDs (S1, A2, Q4…) in commits, MR text, docs or
  data files; use descriptive names. Keep MR descriptions to one screen.
- Commit messages follow `type(scope): description`, one concern per commit.
- Never print secrets. Credentials live in `.env` (mode 0600) and are
  referenced from `apisip.toml` as `${VAR}`.
