# Roadmap

Not in 0.1.0. Each item becomes a GitHub issue labelled `roadmap`.

- **Child streams:** endpoints with path parameters (`/users/{id}/orders`), fed
  by the records of a parent stream. Today `init` lists them as skipped.
- **OAuth2 authorization-code flow:** browser login with a local callback
  server, plus refresh tokens.
- **Reverse mode (`push`):** POST local records to an API, with the same retry
  and throttle runtime.
- **Webhook receiver (`listen`):** a small server that appends received events
  to the same sinks.
- **Iceberg table metadata** over the Parquet output.
- **Config drafted from HTML docs** for APIs that publish no OpenAPI spec.
- **Python bindings, and a Homebrew tap.**
- **Renamed streams in `update`:** a stream the user renamed in `apisip.toml`
  is offered again as a new endpoint, because streams are matched by name. Match
  them by path as well.
- **Cap far-future cursors:** a record whose cursor is far in the future (bad
  source data) moves the watermark past later records, which filter mode then
  never fetches. Cap the watermark at the run time plus a configurable skew.
- **History view after an empty snapshot:** when the last committed
  full-refresh snapshot is empty, the history view has no upper time bound, so
  rows of a later crashed snapshot show up as non-current versions.
- **Hosts allowed to receive credentials:** an allowlist for APIs whose
  next-page links legitimately move to another host, which today fails the
  stream. On an http-to-https redirect, the HTTP client also drops the
  `Authorization` header, so such APIs get a 401 unless the key is in a custom
  header.

## Waiting for a use case

No issue until someone reports the problem they solve.

- **Lookback window:** an optional `lookback = "5m"` per stream that extends
  the fingerprint check at the watermark to a time window, to catch records
  whose transaction committed after a later record was already read.
- **A lease for stores without conditional writes:** only if the plain-write
  fallback proves common.
