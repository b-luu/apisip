# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0](https://github.com/b-luu/apisip/compare/v0.2.0...v0.3.0) - 2026-11-05

### Fixed

- *(http)* keep the address module inside the crate
- *(http)* name one address_class entry in the refusal hint
- *(http)* keep http_client inside the crate
- *(http)* match address_class keys in any case
- *(update)* record the address class of a new host ([#97](https://github.com/b-luu/apisip/pull/97))
- *(http)* still record the base_url host when token_url has an unset variable
- *(update)* record the address class of a new host
- let capped timestamps still choose the cursor kind
- keep far-future cursors from moving the watermark
- *(detect)* key the description fallback and extension walk by content
- *(http)* skip a token_url host filled from ${VAR} and classify NAT64
- *(http)* refuse a configured host whose address class changed

### Other

- Merge remote-tracking branch 'origin/main' into fix/address-class
- *(http)* check that ApiClient::build connects to the checked addresses ([#99](https://github.com/b-luu/apisip/pull/99))
- Fix formatting and enhance contributing guidelines
- Update CONTRIBUTING.md for release process clarity
- *(http)* check that ApiClient::build connects to the checked addresses
- *(detect)* cover the extension walk's depth limit and key filter
- *(nix)* provide dist in the dev shell at the pinned version
- note when a recorded host is resolved
- describe the recorded address class
- *(auth)* share the token_url resolution

## [0.2.0](https://github.com/b-luu/apisip/compare/v0.1.4...v0.2.0) - 2026-11-04

### Fixed

- cap response size and page count for untrusted APIs ([#74](https://github.com/b-luu/apisip/pull/74))
- *(detect)* key the rate-limit parse cache by content
- *(detect)* parse each shared rate-limit description once
- *(http)* name max_response_mb only when the limit is hit
- *(auth)* cap the OAuth2 token response at 1 MB
- *(http)* cap response size and page count per stream
- *(spec)* borrow security schemes and test every inlining limit
- *(spec)* borrow parameters and cap every inlined copy
- *(spec)* bound inlining without losing streams or memory
- *(spec)* count long strings and keys against the inlining budget
- *(spec)* cap $ref inlining at 100,000 nodes

### Other

- *(security)* state the same-host rule for next links and redirects exactly
- *(spec)* pin the inlining ceiling boundary and Operation::param
- Merge remote-tracking branch 'origin/main' into fix/bound-ref-inlining
- fix the e2e doc comment placement and the boundary check
- cover the local commit lock and the boundary cap warning
- *(contributing)* sync dist pins when Dependabot bumps release.yml
- *(update)* keep environment references from the spec out of updates
- *(detect)* cover server-variable substitution in the base URL
- cover max_pages = 0 and a rejected max_response_mb = 0
- keep the unchecked-address gap explicit in SECURITY.md
- pin the limit defaults and the exact-limit boundaries
- *(security)* describe the inlining bounds that remain
- Merge branch 'main' into fix/bound-ref-inlining
- *(spec)* pin the inlining budget and depth limit boundaries
- Merge branch 'main' into fix/bound-ref-inlining
- *(security)* describe the inlining cap and what it leaves open

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
