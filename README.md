# DuckDB Prometheus extension

[![CI](https://github.com/botan/duckdb-prometheus/actions/workflows/MainDistributionPipeline.yml/badge.svg)](https://github.com/botan/duckdb-prometheus/actions/workflows/MainDistributionPipeline.yml)
[![Lifecycle: experimental](https://img.shields.io/badge/lifecycle-experimental-orange.svg)](https://lifecycle.r-lib.org/articles/stages.html#experimental)
[![License](https://img.shields.io/github/license/botan/duckdb-prometheus?color=blue)](LICENSE)

Query Prometheus-compatible HTTP APIs through DuckDB. Select samples with
PromQL; analyze them with SQL.

> [!WARNING]
> Experimental and generated with Codex. Use at your own risk; the API may
> change without notice.

## Quick start

Requires Rust 1.86+, Python 3.10+, and Make. Targets DuckDB v1.5.6.

```shell
git clone --recurse-submodules https://github.com/botan/duckdb-prometheus.git
cd duckdb-prometheus
make configure
make debug
```

Start DuckDB with unsigned extensions enabled:

```shell
duckdb -unsigned
```

Load the debug build:

```sql
LOAD './build/debug/prometheus.duckdb_extension';
```

Query `up` over the last 15 minutes:

```sql
SELECT timestamp, job, instance, value
FROM prometheus_scan(
    'up',
    current_timestamp - INTERVAL '15 minutes',
    current_timestamp,
    endpoint := 'http://localhost:9090',
    step := INTERVAL '1 minute',
    labels := ['job', 'instance']
)
ORDER BY timestamp DESC, job, instance;
```

`prometheus_scan` returns one row per sample. `labels` contains the complete
label map; the `labels` argument also adds selected labels as nullable columns.

To load the local build from Python:

```python
import duckdb

con = duckdb.connect(config={"allow_unsigned_extensions": "true"})
con.execute("LOAD './build/debug/prometheus.duckdb_extension'")
```

## Examples

### Combine PromQL and SQL

Calculate per-job request rates in PromQL, then hourly averages in SQL:

```sql
SELECT
    date_trunc('hour', timestamp) AS hour,
    job,
    avg(value) AS requests_per_second
FROM prometheus_scan(
    'sum by (job) (rate(http_requests_total[5m]))',
    current_timestamp - INTERVAL '24 hours',
    current_timestamp,
    endpoint := 'http://localhost:9090',
    step := INTERVAL '1 minute',
    labels := ['job']
)
GROUP BY ALL
ORDER BY ALL;
```

### Run an instant query

Run an instant query at the Prometheus server's current time:

```sql
SELECT job, instance, value
FROM prometheus_query(
    'up',
    endpoint := 'http://localhost:9090',
    labels := ['job', 'instance']
)
ORDER BY job, instance;
```

Use `evaluation_time` to query a specific time.

### Discover metrics

List metric names:

```sql
SELECT value AS metric
FROM prometheus_label_values(
    '__name__',
    endpoint := 'http://localhost:9090'
)
ORDER BY metric;
```

## Query functions

| Function | Purpose |
|---|---|
| `prometheus_scan(query, start, end, ...)` | Run one range query |
| `prometheus_scan_many(queries, start, end, ...)` | Run several range queries |
| `prometheus_query(query, ...)` | Run one instant query |
| `prometheus_query_many(queries, ...)` | Run several instant queries |

`query` is raw PromQL. `start` and `end` accept `TIMESTAMPTZ`, `TIMESTAMP`, or
timestamp strings. Unzoned values are UTC; bounds are inclusive.

| Named argument | Functions | Type | Default |
|---|---|---|---|
| `endpoint` | All | `VARCHAR` | Required |
| `request_timeout` | All | `INTERVAL` | No total deadline |
| `labels` | All | `VARCHAR[]` | `[]` |
| `step` | Range | `INTERVAL` | `1 minute` |
| `evaluation_time` | Instant | Timestamp or timestamp string | Server time |

`endpoint` may include a proxy path and query parameters. Requests are
unauthenticated. `request_timeout` and `step` must be positive fixed intervals;
months and years are not accepted.

All query functions return:

| Column | Type | Description |
|---|---|---|
| `labels` | `MAP(VARCHAR, VARCHAR)` | Series labels, sorted by key |
| `timestamp` | `TIMESTAMP WITH TIME ZONE` | Absolute sample time |
| `value` | `DOUBLE` | Sample value, including `NaN` and infinities |

Read labels with `labels['job']`. Promote them to columns with
`labels := ['job', 'instance']`; names must be unique and cannot be `labels`,
`timestamp`, or `value`.

Instant vectors, scalars, and matrices use this schema. Plural functions require
a non-empty list, run requests sequentially, and use `UNION ALL` semantics. One
failed request fails the query. `request_timeout` applies per request.

## Discovery functions

All discovery functions require `endpoint` and accept `request_timeout`.

| Function | Result columns |
|---|---|
| `prometheus_labels(...)` | `label VARCHAR` |
| `prometheus_label_values(label, ...)` | `value VARCHAR` |
| `prometheus_series(matcher, ...)` | `labels MAP(VARCHAR, VARCHAR)` |
| `prometheus_metadata(...)` | `metric VARCHAR`, `type VARCHAR`, `help VARCHAR`, `unit VARCHAR` |

`prometheus_labels` and `prometheus_label_values` accept optional `matcher`,
`start_time`, and `end_time`; `prometheus_series` accepts the two time bounds.
`prometheus_metadata` accepts optional `metric`.

## Build and test

`make release` writes `build/release/prometheus.duckdb_extension`.

```shell
make check
make debug
make test_debug
make release
make test_release
```

`make check` runs formatting, Clippy with warnings denied, and Rust tests. The
integration tests use an ephemeral local Prometheus-compatible server.

## Limitations

- DuckDB filters and aggregations are not pushed into PromQL.
- Responses have no extension-imposed byte, series, or sample limit. Singular
  functions buffer one response; plural functions buffer one response at a
  time. Large responses can exhaust process memory.
- Native histogram samples and instant-query string results are not supported.
- Prometheus warnings are not exposed.
- Authentication, custom headers, retries, time-range chunking, paging, and
  caching are not implemented.
- Discovery functions accept one matcher, not repeated `match[]` values.
- The blocking HTTP client does not support WASM.

## License

Apache License 2.0. See [LICENSE](LICENSE).
