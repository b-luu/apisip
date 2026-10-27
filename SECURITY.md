# Security policy

## Supported versions

Only the latest 0.x release gets security fixes.

## Report a vulnerability

Do not open a public issue. Use GitHub's private reporting instead:
go to the [Security tab](https://github.com/b-luu/apisip/security) and choose
"Report a vulnerability".

Include the apisip version, what you did, and what you saw. Do not include
real credentials.

You can expect an answer within 7 days. If the report is valid, we agree on a
fix and a disclosure date with you, and we credit you in the release notes
unless you prefer not to be named.

## Scope

apisip sends the credentials in `.env` to the API in `apisip.toml`. Reports
about leaking those credentials (in logs, error messages, redirects or
next-page links to other hosts) are in scope.

## Trust model

apisip runs with your credentials and writes what it receives. Point it only
at specs and APIs you trust. This section says what each input can make
apisip do, what apisip guards against, and what it does not.

### The spec

The spec decides where requests go (`servers`), which auth schemes apply, and
where OAuth2 fetches its token (`tokenUrl`). `init` copies these into
`apisip.toml`, so read the `base_url` and any `token_url` there before the
first `run`. Use the spec published by the API's vendor, not a copy from an
unknown source.

apisip reads only local `$ref`s (inside the same document) and fetches no
other URL from the spec. Recursive refs are cut. Known gap: a spec whose refs
branch at every level can make inlining use all available memory
([#38](https://github.com/b-luu/apisip/issues/38)).

### Credentials

- Credentials go only to the API's own host, over the same scheme and port or
  upgraded to https. A redirect or a next-page link to another host fails the
  stream instead of receiving them. The exception is the OAuth2 client id and
  secret, which go to the `token_url`, possibly on another host.
- Credentials and values read from the environment are masked in request
  URLs, logs and HTTP error messages.
- `init` lists `.env` in `.gitignore`. Keep it out of version control, and
  give apisip a key with read access only when the API offers one.

### API responses

- Records are written as received (JSONL) or typed from the schema
  (Parquet). apisip does not execute anything from a response.
- A page that repeats the previous one stops pagination.
- Known gaps: base URLs and next-page links are not checked against
  loopback, private or cloud metadata addresses, a response is read whole
  into memory, and there is no limit on the number of pages
  ([#39](https://github.com/b-luu/apisip/issues/39)). Run apisip against
  untrusted APIs only on a machine with no internal services to reach.

### The destination

- The output contains whatever the API returned, which can include personal
  data. Protect the destination as you protect the source system.
- The DuckDB views in `_apisip/views/` are SQL files that you run with
  `.read`. Anyone who can write to the destination can change them, so limit
  write access to the destination to apisip and its operators.
- On S3, give apisip credentials limited to its own prefix (it reads,
  writes and deletes there).

### Installing

- `cargo install apisip` builds the crate published on crates.io. Since
  0.1.1, releases are published from this repository's CI through crates.io
  trusted publishing, not with a personal token.
- The shell installer and the release archives are downloaded over HTTPS
  from GitHub releases. Each archive has a `.sha256` file:

  ```sh
  sha256sum -c apisip-x86_64-unknown-linux-musl.tar.xz.sha256
  ```

  The checksum detects a corrupted download, not a tampered release, because
  it comes from the same place. Signed releases are planned
  ([#35](https://github.com/b-luu/apisip/issues/35)).
