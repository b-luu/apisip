# Roadmap

Not in v1. Each item becomes a GitHub issue labelled `roadmap`.

- **Child streams:** endpoints with path parameters (`/users/{id}/orders`), fed
  by the records of a parent stream. Today `init` lists them as skipped.
- **OAuth2 authorization-code flow:** browser login with a local callback
  server, plus refresh tokens.
- **Reverse mode (`push`):** POST local records to an API, with the same retry
  and throttle runtime.
- **Webhook receiver (`listen`):** a small server that appends received events
  to the same sinks; then WebSocket sources.
- **Iceberg table metadata** over the Parquet output.
- **Config drafted from HTML docs** for APIs that publish no OpenAPI spec.
- **Python bindings, and releases** on crates.io and Homebrew.
- **Renamed streams in `update`:** a stream the user renamed in `apisip.toml`
  is offered again as a new endpoint, because streams are matched by name. Match
  them by path as well.
- **One lock per destination:** `run` locks its folder, but two folders that
  write to the same destination path are not detected.
- **Cap far-future cursors:** a record whose cursor is far in the future (bad
  source data) moves the watermark past later records, which filter mode then
  never fetches. Cap the watermark at the run time plus a configurable skew.
- **History view after an empty snapshot:** when the last committed
  full-refresh snapshot is empty, the history view has no upper time bound, so
  rows of a later crashed snapshot show up as non-current versions.
- **Minimum Rust version in CI:** build with Rust 1.89, the declared
  `rust-version`.
