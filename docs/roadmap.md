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
