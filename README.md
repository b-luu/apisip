# apisip

Paste the URL of an API's OpenAPI spec, fill in your credentials, and get
every data endpoint as JSONL or Parquet files, on local disk or in any
S3-compatible bucket.

```sh
mkdir my-api && cd my-api            # one folder per API
apisip init https://example.com/openapi.json
apisip run
```

apisip reads the spec once, at `init`. It detects:

- the list endpoints;
- where the records sit in the response;
- the pagination style;
- the auth schemes;
- the published rate limits;
- the best way to fetch only what changed since the last run.

It writes this down as a plain `apisip.toml` you can read and edit, and puts
your secrets in `.env`. After that, `apisip run` downloads everything. Runs
are incremental by default, with retries, throttling and a staged commit, so a
crash never leaves half a run behind.

It is a single binary written in Rust. No Python, no platform, no daemon.

## Contents

- [Install](#install)
- [Quickstart with the bundled mock API](#quickstart-with-the-bundled-mock-api)
- [One folder per API](#one-folder-per-api)
- [Commands](#commands)
- [Configuration](#configuration)
- [Understanding the output](#understanding-the-output)
- [Types come from the spec](#types-come-from-the-spec)
- [Following API changes with `update`](#following-api-changes-with-update)
- [Alternatives](#alternatives)
- [Works well with](#works-well-with)
- [Roadmap](#roadmap) and [License](#license)

## Install

From a clone of this repository:

```sh
cargo install --path .
```

With Nix, the flake provides the package, an app and a dev shell:

```sh
nix run . -- --help
nix develop        # cargo, clippy, rustfmt, rust-analyzer and duckdb
```

## Quickstart with the bundled mock API

The repository ships a small fake shop API with its own spec. Use it to try
apisip without an account anywhere. In one terminal:

```sh
cargo run --example mock-api          # serves http://127.0.0.1:8080, token: mock-token
```

In another:

```sh
mkdir mockshop && cd mockshop
apisip init http://127.0.0.1:8080/openapi.json
```

`init` asks for the base URL (default: the spec's server), where to write data
(default: this folder), the output format, the auth scheme and the token. Type `mock-token` at the token prompt. It is stored
in `.env` with mode 600 and never echoed. Then it lists what it found:

```text
4 stream(s) enabled:
  users                    filter        page number   key id
  orders                   early stop    offset        key id
  events                   early stop    cursor        key id
  plans                    full refresh  next url      key id

Skipped:
  /users/{id}                            needs a parent stream (path parameters)

Rate limit: 1000 calls per 1m.
```

Check the setup, then download:

```sh
apisip check     # one request per stream, writes nothing
apisip run
```

```text
  events                   early stop          20 records    1 file(s)
      warning: the cursor went up under a descending sort: the API does not sort as documented. ...
  orders                   early stop          30 records    1 file(s)
  plans                    full refresh         6 records    1 file(s)
  users                    filter              25 records    1 file(s)
      drift: undeclared field nickname (1 records), e.g. "seven"
      drift: age: expected integer, got "41" (1 records)
```

The mock API lies on purpose:

- **events:** it claims a descending sort but does not honour it, so apisip
  falls back to a full fetch for that stream;
- **users:** one user carries a field the spec does not declare, and another
  has a mistyped value. Both are reported as drift.

Run `apisip run` again: `users` and `orders` now fetch only what changed. Query
the result with [DuckDB](https://duckdb.org):

```sh
duckdb -c ".read _apisip/views/all.sql" -c "select * from users_current limit 5"
```

## One folder per API

apisip works like git: each folder holds one API. Every command reads
`apisip.toml` in the current directory, or in the folder given with `-C`:

```sh
apisip -C ~/data/mockshop run
```

`init` writes:

| File | Commit it? | What |
|---|---|---|
| `apisip.toml` | yes | streams, pagination, auth references, destination |
| `.env` | **no** | secrets, mode 600; `.gitignore` lists it |
| `.env.example` | yes | the variable names, empty |
| `.apisip/` | yes | the spec snapshot and baseline used by `update` |

`init` refuses to overwrite an existing `apisip.toml` without `--force`.

## Commands

All subcommands are verbs.

| Command | What it does |
|---|---|
| `apisip init <spec>` | Reads a spec (URL or file, JSON or YAML; OpenAPI 3.0, 3.1 or Swagger 2.0), asks a few questions, writes the folder. |
| `apisip list` | The configured streams, their mode, pagination, schema source and key. |
| `apisip check` | A dry run: validates the config, the credentials and the destination with one request per stream. Writes nothing. |
| `apisip run` | Downloads every enabled stream. `--stream a,b`, `--full-refresh`, `--strict`. |
| `apisip update [<spec>]` | Diffs a new version of the spec against the config and merges it. `--check` only prints the diff. |

Every prompt has a default and a matching flag, so `init --yes` runs without a
terminal, for scripts and CI. Credentials are then read from the environment:

```sh
MOCKSHOP_TOKEN=mock-token apisip init --yes http://127.0.0.1:8080/openapi.json \
  --auth bearerAuth --format parquet --destination s3://my-bucket/mockshop
```

## Configuration

`init` generates `apisip.toml`, and you can edit it by hand. Here is an excerpt
from the mock API:

```toml
[source]
spec = "http://127.0.0.1:8080/openapi.json"
base_url = "http://127.0.0.1:8080"
auth = { type = "bearer", scheme = "bearerAuth", token = "${MOCKSHOP_TOKEN}" }
rate_limit = { max_calls = 1000, period = "1m" }
retry = { max_tries = 8, max_backoff = "1m", on_status = [429, 500, 502, 503, 504] }
concurrency = 4

[destination]
url = "file://."                  # or s3://bucket/prefix
format = "jsonl"                  # or parquet
rotate = { max_bytes = "128MiB" }

[[streams]]
name = "users"
path = "/users"
records = "/data"                 # JSON pointer to the record array
primary_key = "id"
pagination = { type = "page_number", param = "page", size_param = "per_page", size = 10, start = 1 }
incremental = { cursor = "updated_at", param = "updated_since" }
```

- **Secrets:** `${VAR}` is read from `.env`, then from the process
  environment, which wins.
- **Auth:** bearer, API key (header, query or cookie), HTTP basic, and OAuth2
  client credentials. OAuth2 fetches a token from the spec's `tokenUrl`, caches
  it, and refreshes it on expiry or on a 401.
- **Pagination:** page number, offset, cursor, next URL, and `Link: rel="next"`
  headers.
- **Rate limits:** OpenAPI has no standard field for rate limits. `init` reads
  them from vendor extensions (`x-ratelimit`), declared rate-limit headers, or
  description text such as "100 requests per minute". When the spec lists
  several plans, `init` asks which one you are on. When it finds nothing, it
  says so and shows the line to add. At run time, `Retry-After` and the
  `X-RateLimit-*` / `RateLimit-*` headers always take over.
- **S3:** the standard variables `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
  `AWS_REGION`, and `AWS_ENDPOINT_URL` for S3-compatible stores (R2, Garage,
  SeaweedFS, Ceph...). `init` asks for them when the destination is `s3://`.

### Incremental modes

`run` is incremental by default. Each stream keeps a watermark, the highest
cursor value seen, and `--full-refresh` ignores it. `init` gives each stream
one of three modes:

1. **filter:** the API has a filter parameter (`since`, `updated_after`...).
   The next run sends it with the watermark.
2. **early stop:** there is no filter, but the API sorts by the cursor in
   descending order. The run stops at the first page older than the watermark.
   If the cursor ever goes up during a run, the API is not sorted as it claims.
   The stream then fetches everything, and `list` shows
   `early stop (unreliable)`.
3. **full refresh:** nothing usable exists, so every run takes a complete
   snapshot.

The comparison with the watermark is inclusive, so delivery is **at least
once**. Losing a record at the boundary is worse than repeating one.

## Understanding the output

apisip never updates or deletes a row it has written. Each run appends what it
captured:

```text
users/dt=2026-10-16/users-<run_id>-0000.jsonl
plans/dt=2026-10-16/run_id=<run_id>/plans-<run_id>-0000.jsonl     # full refresh: one snapshot per run
users/_quarantine/<run_id>.jsonl                                  # Parquet only: records that break the spec
_apisip/state.json                                                # watermarks
_apisip/runs/<run_id>.json                                        # manifest: counts, files, ranges, drift
_apisip/views/*.sql                                               # generated views
```

Every record carries `_apisip_run_id` and `_apisip_extracted_at`.

**The grain is one row per version that a run observed.** Take a user who is
created, then updated twice, with a run after each change: that user appears
three times. If the user changes five times between two runs, only the latest
state is captured, once. The output is a history of snapshots taken at run
times, not a change-data-capture feed.

Duplicates come from the inclusive boundary, from re-running after a failure,
and from full-refresh snapshots. Each stream is written to a staging area, then
committed in this order:

1. move the files into place;
2. advance the watermark;
3. write the manifest.

A failed run therefore leaves no data and no moved watermark. The manifest
flags any overlap with an earlier run.

### Generated views

After each run, apisip writes two DuckDB views per stream that has a primary
key, plus `all.sql` that creates them all:

- **`<stream>_current`:** one row per key, its latest version.
- **`<stream>_history`:** one row per distinct version, with `valid_from`,
  `valid_to` and `is_current`: a type 2 slowly changing dimension. For
  full-refresh streams, it compares consecutive snapshots.

```sh
duckdb -c ".read _apisip/views/all.sql" \
  -c "select id, name, valid_from, valid_to, is_current from users_history where id = 3"
```

```text
│ 3 │ user 3          │ 2026-01-01 00:03:00 │ 2026-01-01 01:16:00 │ false │
│ 3 │ user 3 (edited) │ 2026-01-01 01:16:00 │ NULL                │ true  │
```

The views use only standard window functions. Here is `users_current`:

```sql
select * exclude (_apisip_rn) from (
  select *, row_number() over (partition by "id" order by "updated_at" desc nulls last, _apisip_extracted_at desc) as _apisip_rn
  from read_json('/data/mockshop/users/dt=*/**/*.jsonl', format = 'newline_delimited', union_by_name = true, hive_partitioning = false)
) where _apisip_rn = 1
```

To port it to a warehouse, replace the `read_json` / `read_parquet` call with
your external table. Replace `select * exclude (...)` with the column list, or
use `qualify row_number() over (...) = 1` where the engine supports it. For an
S3 destination, the views point at the `s3://` paths. Load DuckDB's `httpfs`
and create an S3 secret first.

## Types come from the spec

The spec is the source of truth. Parquet columns come from the response
schema, before any request is made, so types are stable across files and runs:

| Spec | Parquet |
|---|---|
| `integer` / `number` / `boolean` | Int64 / Float64 / Boolean |
| `string` + `format: date-time` / `date` | Timestamp (µs, UTC) / Date32 |
| other strings, enums | Utf8 |
| object with properties / array | Struct / List |
| `oneOf`, `anyOf`, free-form object | JSON string |

All columns are nullable. Fields the spec does not declare go into one
`_apisip_extra` JSON column, so no data is lost.

A record whose value breaks the spec, such as `"41"` where `integer` is
declared, is never silently cast. It goes to `<stream>/_quarantine/` with the
reason, and is counted as drift. `run --strict` fails instead. JSONL keeps
records exactly as received: drift is reported, and nothing is quarantined.

When an endpoint declares JSON but no schema, the types are inferred from the
first records, and `list` shows `schema: inferred`.

## Following API changes with `update`

```sh
apisip update                         # re-reads the spec URL in apisip.toml
apisip update https://example.com/v2/openapi.json
apisip update --check                 # prints the diff, exits 1 if anything changed (for CI)
```

`update` compares three versions: the spec at `init` (kept in `.apisip/`), the
new spec, and your current `apisip.toml`. Then it applies these rules:

- **A value you never edited** follows the spec.
- **A value both you and the spec changed** is a conflict. `update` shows both
  values and asks; `--yes` keeps yours.
- **Removed endpoints** are disabled, not deleted, so their data and watermark
  stay.
- **New endpoints** are offered, unselected by default.
- **Auth** is always re-confirmed. Enter keeps the current scheme and each
  current secret, shown masked.
- **Response schema changes** are listed field by field (`+ phone (string)`),
  along with the drift seen by the last run.

Comments and formatting in `apisip.toml` are kept.

## Alternatives

apisip is young, and other tools may fit better. Facts checked in October
2026; follow the links for the current state.

| Tool | Runtime | Built from a spec or docs? | Output | Types |
|---|---|---|---|---|
| [dlt](https://dlthub.com/docs/dlt-ecosystem/verified-sources/rest_api) `rest_api` + [`dlt-init-openapi`](https://dlthub.com/docs/dlt-ecosystem/verified-sources/openapi-generator) | Python library | OpenAPI 3.x to Python config (a proof of concept) | warehouses, files, S3 | inferred |
| [dltHub AI Workbench](https://dlthub.com/docs/hub/ingestion/rest-api-source) | Python + coding agent | API docs to config | as dlt | inferred |
| [Airbyte Connector Builder AI Assist](https://docs.airbyte.com/platform/connector-development/connector-builder-ui/ai-assist) | Airbyte platform | docs or OpenAPI URL (beta) | warehouses, S3 | inferred or manual |
| [Fivetran AI Connector Agent](https://fivetran.com/docs/changelog/2026/may-2026) | SaaS | REST API sources to a hosted connector (beta) | warehouses | managed |
| [Meltano](https://meltano.com) / Singer REST taps | Python | no, hand-written config | Singer targets | inferred or manual |
| [Sling](https://docs.slingdata.io/concepts/api-specs) | Go binary | its own hand-written YAML API spec (paid plans) | databases, files | inferred |
| [ingestr](https://github.com/bruin-data/ingestr), [CloudQuery](https://www.cloudquery.io) | Python / Go | no, a fixed list of sources | databases, files | inferred or plugin |
| [Redpanda Connect](https://docs.redpanda.com/redpanda-connect/), [Vector](https://vector.dev) | Go / Rust binaries | no, hand-written `http_client` | many sinks | none |
| **apisip** | Rust binary | OpenAPI to config, with an `update` merge | JSONL / Parquet, local or S3 | **from the spec** |

Choose apisip when you want one binary that reads OpenAPI, Parquet typed from
the spec rather than from the data, a merge when the spec changes, and an
append-only history with generated views.

Choose something else in these cases:

- **You are happy with Python:** dlt covers most of this already, and many
  destinations.
- **The API publishes no spec:** Airbyte, Fivetran and dltHub build connectors
  from HTML docs.
- **You need many non-HTTP sources:** use Sling, ingestr or CloudQuery.

## Works well with

- **[DuckDB](https://duckdb.org):** reads the JSONL and Parquet output
  directly, locally or on S3 through `httpfs`. The generated views are DuckDB
  SQL.
- **[dbt](https://www.getdbt.com) / [SQLMesh](https://sqlmesh.com):** build
  downstream models on the `_current` and `_history` views, or on your own
  version of them.
- **Warehouses that read Parquet from S3:** Snowflake, BigQuery, Databricks,
  Redshift Spectrum, ClickHouse and Trino. Point an external table at
  `s3://bucket/prefix/<stream>/dt=*/`.
- **[Apache Iceberg](https://iceberg.apache.org):** Iceberg metadata over the
  Parquet output is on the roadmap. Until then, add the files to a table with
  your engine's `add_files` procedure.
- **Schedulers:** cron, systemd timers, GitHub Actions, Airflow, Dagster.
  `apisip run` exits non-zero when a stream fails.

## Roadmap

See [docs/roadmap.md](docs/roadmap.md). Planned work includes:

- child streams (endpoints with path parameters);
- the OAuth2 authorization-code flow;
- POSTing data back to an API;
- a webhook receiver;
- Iceberg metadata.

The design is described in [docs/design.md](docs/design.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
