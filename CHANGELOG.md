# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.4](https://github.com/b-luu/apisip/compare/v0.1.3...v0.1.4) - 2026-10-30

### Fixed

- *(ci)* sync dist's checkout pin with the Dependabot bump
- *(spec)* parse YAML specs with serde-saphyr instead of serde_yaml_ng ([#64](https://github.com/b-luu/apisip/pull/64))
- *(spec)* parse YAML specs with serde-saphyr instead of serde_yaml_ng
- *(http)* strip control characters from response snippets in errors
- *(update)* count recent runs without drift when offering fields
- *(config)* allow only letters, digits, `_` and `-` in stream names

### Other

- *(deps)* bump tokio
- *(http)* cover the 300-character cut in response snippets
- *(update)* cover a run whose records were all quarantined
- *(security)* list auth scheme names and cursor values in the known gap
- *(security)* narrow the control-character known gap to field names

## [0.1.3](https://github.com/b-luu/apisip/compare/v0.1.2...v0.1.3) - 2026-10-29

### Fixed

- *(run)* re-check destination ownership at commit
- *(auth)* mask the token URL in the OAuth2 error message
- *(prompt)* keep the real default when the user accepts its cleaned display
- *(init)* strip control characters from spec text before printing it

### Other

- Potential fix for pull request finding 'CodeQL / Failure to use HTTPS URLs'
- Merge branch 'main' into fix/mask-token-url
- *(run)* read the folder id once per run for the owner check
- *(auth)* use https URLs in the authenticator tests
- *(security)* say what an S3 endpoint receives
- *(security)* apply review notes to the trust model
- *(security)* cover environment references and other spec-driven risks
- *(security)* describe the trust model and its known gaps

## [0.1.2](https://github.com/b-luu/apisip/compare/v0.1.1...v0.1.2) - 2026-10-28

### Fixed

- *(init)* strip control characters where spec text enters a comment
- *(detect)* keep spec titles and paths from breaking out of config comments
- *(detect)* never copy a ${VAR} reference from the spec into apisip.toml

## [0.1.1](https://github.com/b-luu/apisip/compare/v0.1.0...v0.1.1) - 2026-10-27

### Fixed

- *(deps)* bump zstd crates for RUSTSEC-2026-0391, 0398, 0403, 0410
- *(http)* count rate-limit reset waits toward retry.max_wait

### Other

- *(readme)* hide the Scorecard badge until the score recovers
- *(readme)* add CI, crates.io, MSRV and license badges ([#32](https://github.com/b-luu/apisip/pull/32))
- *(readme)* link the launch article ([#30](https://github.com/b-luu/apisip/pull/30))
- *(readme)* add CI, crates.io, MSRV and license badges
- *(readme)* link the launch article
- *(http)* bound the max_tries test so a retry loop fails fast
- *(contributing)* first release is done, trusted publishing is configured
- *(roadmap)* keep the bold on the topic only
- *(roadmap)* drop crates.io releases, shipped in 0.1.0

## [0.1.0](https://github.com/b-luu/apisip/releases/tag/v0.1.0) - 2026-10-23

First release. `apisip init` reads an OpenAPI or Swagger spec and writes
`apisip.toml` and `.env`. `apisip run` downloads every enabled stream to JSONL or
Parquet, on local disk or S3, with watermarks and a staged commit, so a failed
run leaves no data and moves no watermark. Parquet types come from the spec, and
generated DuckDB views give the current state and the history.
