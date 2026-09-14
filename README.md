# ladybug-adbc (Rust)

Arrow Flight / ADBC columnar interface for LadybugDB — Rust port of
[`ladybug-adbc-python`](../ladybug-adbc-python). Serves LadybugDB Cypher
results as native Arrow columnar batches over gRPC, consumed with the standard
columnar.tech call pattern — no row-by-row JSON.

## Install

```bash
cargo build --release
```

## Run

```bash
# 1. server (:memory: demo graph, same data as the Python package)
./target/release/ladybug-flight-server
# grpc://127.0.0.1:50051, try: --db /tmp/graph.lbdb --port 50051 --no-demo

# 2. client — ADBC-equivalent prepared-statement flow (default)
./target/release/ladybug-client
./target/release/ladybug-client --demo            # all 6 demo queries
./target/release/ladybug-client --demo --no-adbc  # plain FlightClient path
```

## The recipe, adapted

```rust
use ladybug_adbc::LadybugClient;

let mut client = LadybugClient::connect("grpc://localhost:50051").await?;
let (schema, batches) = client
    .query_flightsql("MATCH (u:User) RETURN u.name, u.age ORDER BY u.id")
    .await?;
```

`query_flightsql` is wire-for-wire what `adbc_driver_flightsql.dbapi` does:
`CreatePreparedStatement` action → `GetFlightInfo` with
`CommandPreparedStatementQuery` → `DoGet` with `TicketStatementQuery` →
`ClosePreparedStatement`. The `query` field is just a string on the wire, so
Cypher passes straight through. `query_flight` is the plain-Flight equivalent
(`GetFlightInfo(new_cmd(cypher))` → `DoGet`).

The two servers are wire-compatible: the Python ADBC client queries this Rust
server and the Rust client queries the Python server (verified for all demo
queries, both flows).

## Benchmark (throughput)

Two ways to measure — both report queries/s, rows/s, MiB/s:

```bash
# Standalone throughput table (spawns an ephemeral in-process server by default,
# or hammers an existing one with --uri; use separate processes for real numbers)
./target/release/ladybug-bench
./target/release/ladybug-bench --uri grpc://localhost:50051 --iterations 500 --concurrency 8
./target/release/ladybug-bench --query all_users --no-adbc

# Criterion micro-benchmarks: direct (no gRPC) vs plain Flight vs prepared flow
cargo bench
```

Example (release, server + bench as separate processes, concurrency 8):

| query                | qps    | rows/s |
|----------------------|--------|--------|
| all_users            | ~1700  | ~8500  |
| follows / lives_in   | ~190   | ~1000  |
| two_hop / same_city  | ~80–130| ~150–260|

## SQL vs Cypher

`adbc_driver_flightsql` is *typically* tied to SQL (`cursor.execute(sql)`,
`CommandStatementQuery{query}`), but `query` is just a string on the wire.
`LadybugFlightServer` treats it as **Cypher** (see `extract_query()`), so the
columnar.tech call pattern works unchanged for a graph DB. `CALL show_tables()`
keeps working for schema introspection.

## REST vs Arrow

| | REST (Flask) | Arrow (this crate / Python twin) |
|---|---|---|
| encoding | `to_pydict` → `json.dumps` → parse | Arrow IPC over gRPC |
| types | lost (everything is JSON) | preserved (`schema`, native batches) |
| client | `POST /query {"data": [[0, cypher]]}` | prepared statement / `new_cmd(cypher)` |
| batching | manual row-index envelope | `FlightDataEncoder` stream |

## Files

- `src/lib.rs` — `demo_queries()`, `build_demo_graph()`, `execute_cypher()`,
  dependency-free Flight SQL protobuf codec (`extract_query`, …).
- `src/server.rs` — `LadybugFlightServer` (`FlightService` + Flight SQL
  prepared-statement handshake), `serve_ephemeral()` test helper.
- `src/client.rs` — `LadybugClient` (persistent connection) plus
  `query_cypher_flight()` / `query_cypher_flightsql()` one-shot functions.
- `src/main.rs` — `ladybug-flight-server` binary (`--host/--port/--db`,
  `FLIGHT_HOST`/`FLIGHT_PORT`/`LADYBUG_DB` env).
- `src/bin/client.rs` — `ladybug-client` demo.
- `src/bin/bench.rs` — `ladybug-bench` throughput table.
- `src/bin/healthcheck.rs` — container `HEALTHCHECK` probe.
- `benches/throughput.rs` — criterion benches (direct / plain / prepared).
- `tests/test_flight.rs` — ephemeral server, FlightSQL ≡ Flight Arrow tables.

## Docker (GHCR)

```bash
docker build -t ladybug-adbc-rs:dev .

# :memory: demo graph on localhost:50051
docker run --rm -p 50051:50051 ladybug-adbc-rs:dev

# persistent graph via a volume
docker run --rm -p 50051:50051 -v ladybug-data:/data \
  -e LADYBUG_DB=/data/graph.lbdb ladybug-adbc-rs:dev
```

Env knobs: `FLIGHT_HOST` (default `0.0.0.0` in the image), `FLIGHT_PORT`
(default `50051`), `LADYBUG_DB` (default `:memory:`).
