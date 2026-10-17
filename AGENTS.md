# Notes for coding agents

This file is a thin entry point. The real content lives in three documents.
Read them before you change anything:

- [README.md](README.md): what apisip does and how to use it. It covers the
  commands, `apisip.toml`, the output layout and the generated views.
- [CONTRIBUTING.md](CONTRIBUTING.md): how to build, test and send changes. It
  covers the gates, the optional suites (live APIs, S3) and commit conventions.
- [docs/design.md](docs/design.md): how it works inside. Read it before you
  change detection, the commit protocol, incremental modes or the output
  layout.

[docs/roadmap.md](docs/roadmap.md) lists planned work that is out of scope
unless the task says otherwise.

## Project context in one paragraph

apisip is one Rust crate, a library plus the `apisip` binary. `init` reads an
OpenAPI or Swagger spec and writes `apisip.toml` and `.env` into the current
folder, one folder per API. `run` downloads every enabled stream to JSONL or
Parquet, locally or on S3, with watermarks and a staged commit, so a failed
run leaves no data and no moved watermark. The spec is the source of truth for
Parquet types. Output is append-only, and the generated DuckDB views give the
current state and the history.

## Rules for agents

These add to CONTRIBUTING.md. They cover mistakes agents make more often than
people do.

- **Run the gates yourself** (`cargo fmt --check`, `cargo clippy --all-targets
  -- -D warnings`, `cargo test`) and report their real output. Do not claim a
  change works because it compiles.
- **Exercise the binary.** For a behaviour change, run `apisip` against the
  mock API (`cargo run --example mock-api`), not only the unit tests.
- **Prove each regression test:** revert the fix, see the test fail, restore
  it, see it pass. A test written to match the code you just wrote often
  locks the bug in.
- **Secrets:** never print, log or commit a credential. Do not read a user's
  `.env`. Any new message that shows a URL, header or response body must go
  through the masking in `src/http/mod.rs` (`ApiClient::shown`, `mask`).
- **Credentials stay on the API's host.** Do not loosen the same-host rule for
  redirects and next-page links without an explicit request.
- **Network:** the default suite is offline. Do not make it depend on the
  network; live tests stay `#[ignore]`d.
- **No private references.** Never name private, client or employer projects
  in code, comments, docs, fixtures, commit messages or issues. Public
  open-source projects, formats and warehouses are fine to name.
- **No plan or work-item IDs** (S1, A2, Q4…) in commits, PR text, docs or data
  files; use descriptive names. Keep PR descriptions to one screen.
- **Stay in scope.** Do not refactor, rename or add features beyond the task,
  and match the comment density and naming of the code around your change.
- **Do not edit `Cargo.lock` by hand,** and do not commit generated output
  (`target/`, run folders, `data/`, `_apisip/`).
