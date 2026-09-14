//! Throughput benchmarks: direct (in-process LadybugDB, no gRPC) vs
//! Flight round-trips (plain + ADBC-equivalent prepared flow).
//!
//! ```sh
//! cargo bench
//! cargo bench -- --quick
//! ```
//!
//! Criterion reports ns/iter plus rows/s (`Throughput::Elements`); run
//! `cargo run --release --bin ladybug-bench` for the qps / rows/s / MiB/s table.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ladybug_adbc::{demo_queries, CHUNK_SIZE};

fn bench_direct(c: &mut Criterion) {
    let mut group = c.benchmark_group("direct");
    group.measurement_time(Duration::from_secs(5));
    for (label, cypher) in demo_queries() {
        // Fresh DB per query so group setup stays outside the measurement loop.
        let db = lbug::Database::in_memory(lbug::SystemConfig::default()).unwrap();
        let conn = lbug::Connection::new(&db).unwrap();
        ladybug_adbc::build_demo_graph(&conn).unwrap();
        let rows: usize = {
            let (_, batches) =
                ladybug_adbc::execute_cypher(&conn, cypher, CHUNK_SIZE).unwrap();
            batches.iter().map(|b| b.num_rows()).sum()
        };
        group.throughput(Throughput::Elements(rows as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &cypher, |b, cypher| {
            b.iter(|| {
                let (_, batches) =
                    ladybug_adbc::execute_cypher(&conn, cypher, CHUNK_SIZE).unwrap();
                criterion::black_box(batches);
            });
        });
    }
    group.finish();
}

fn rt_and_server() -> (tokio::runtime::Runtime, String) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let uri = rt
        .block_on(ladybug_adbc::serve_ephemeral(":memory:", true))
        .map(|(uri, _handle)| uri);
    // Keep the server task alive: intentionally leak the JoinHandle by
    // forgetting it inside the runtime (the runtime outlives the group).
    let uri = uri.unwrap();
    // NOTE: dropping the JoinHandle does not stop the server task.
    (rt, uri)
}

fn bench_flight(c: &mut Criterion) {
    let mut group = c.benchmark_group("flight_plain");
    group.measurement_time(Duration::from_secs(5));
    let (rt, uri) = rt_and_server();
    for (label, cypher) in demo_queries() {
        let rows = rt.block_on(direct_rows_async(&uri, cypher, false));
        group.throughput(Throughput::Elements(rows as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &cypher, |b, cypher| {
            // One persistent connection: measure server throughput, not handshakes.
            let client = rt
                .block_on(ladybug_adbc::LadybugClient::connect(&uri))
                .unwrap();
            let client = tokio::sync::Mutex::new(client);
            b.to_async(&rt).iter(|| async {
                let res = client.lock().await.query_flight(cypher).await.unwrap();
                criterion::black_box(res);
            });
        });
    }
    group.finish();
}

fn bench_flightsql(c: &mut Criterion) {
    let mut group = c.benchmark_group("flight_adbc_prepared");
    group.measurement_time(Duration::from_secs(5));
    let (rt, uri) = rt_and_server();
    for (label, cypher) in demo_queries() {
        let rows = rt.block_on(direct_rows_async(&uri, cypher, true));
        group.throughput(Throughput::Elements(rows as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &cypher, |b, cypher| {
            // One persistent connection: measure server throughput, not handshakes.
            let client = rt
                .block_on(ladybug_adbc::LadybugClient::connect(&uri))
                .unwrap();
            let client = tokio::sync::Mutex::new(client);
            b.to_async(&rt).iter(|| async {
                let res = client.lock().await.query_flightsql(cypher).await.unwrap();
                criterion::black_box(res);
            });
        });
    }
    group.finish();
}

async fn direct_rows_async(uri: &str, cypher: &str, flightsql: bool) -> usize {
    let (_, batches) = if flightsql {
        ladybug_adbc::query_cypher_flightsql(uri, cypher).await.unwrap()
    } else {
        ladybug_adbc::query_cypher_flight(uri, cypher).await.unwrap()
    };
    batches.iter().map(|b| b.num_rows()).sum()
}

criterion_group!(benches, bench_direct, bench_flight, bench_flightsql);
criterion_main!(benches);
