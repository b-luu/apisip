# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/b-luu/apisip/compare/v0.1.0...v0.1.1) - 2026-10-26

### Other

- *(contributing)* first release is done, trusted publishing is configured
- *(roadmap)* keep the bold on the topic only
- *(roadmap)* drop crates.io releases, shipped in 0.1.0

## [0.1.0](https://github.com/b-luu/apisip/releases/tag/v0.1.0) - 2026-10-23

First release. `apisip init` reads an OpenAPI or Swagger spec and writes
`apisip.toml` and `.env`. `apisip run` downloads every enabled stream to JSONL or
Parquet, on local disk or S3, with watermarks and a staged commit, so a failed
run leaves no data and moves no watermark. Parquet types come from the spec, and
generated DuckDB views give the current state and the history.
