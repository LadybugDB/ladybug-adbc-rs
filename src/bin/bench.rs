//! `ladybug-bench`: measure Flight query throughput (qps, rows/s, MiB/s).
//!
//! Spins up an in-process ephemeral server by default, or hammers an existing
//! one with `--uri`:
//!
//! ```sh
//! ladybug-bench                              # ephemeral server, all demo queries
//! ladybug-bench --uri grpc://localhost:50051 # existing server
//! ladybug-bench --query all_users --iterations 1000 --concurrency 8
//! ```

use std::time::Instant;

use clap::Parser;
use ladybug_adbc::{demo_queries, table_stats, LadybugClient};

#[derive(Parser, Debug)]
#[command(name = "ladybug-bench", about = "LadybugDB Flight throughput benchmark")]
struct Args {
    /// Flight server URI. If absent, an ephemeral in-process server is started.
    #[arg(long)]
    uri: Option<String>,
    /// Which query to run: a demo label or "all".
    #[arg(long, default_value = "all")]
    query: String,
    /// Warmup iterations per query (untimed).
    #[arg(long, default_value_t = 20)]
    warmup: usize,
    /// Timed iterations per query.
    #[arg(long, default_value_t = 200)]
    iterations: usize,
    /// Concurrent in-flight queries.
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    /// Use plain Flight instead of the ADBC-equivalent prepared-statement flow.
    #[arg(long, default_value_t = false)]
    no_adbc: bool,
}

struct Row {
    label: String,
    iters: usize,
    secs: f64,
    rows_per_q: usize,
    bytes_per_q: usize,
}

impl Row {
    fn qps(&self) -> f64 {
        self.iters as f64 / self.secs
    }
    fn rows_per_s(&self) -> f64 {
        self.rows_per_q as f64 * self.iters as f64 / self.secs
    }
    fn mib_per_s(&self) -> f64 {
        self.bytes_per_q as f64 * self.iters as f64 / self.secs / (1024.0 * 1024.0)
    }
}

async fn run_once(
    client: &mut LadybugClient,
    cypher: &str,
    flightsql: bool,
) -> anyhow::Result<(usize, usize)> {
    let (schema, batches) = if flightsql {
        client.query_flightsql(cypher).await?
    } else {
        client.query_flight(cypher).await?
    };
    let (rows, _, bytes) = table_stats(&schema, &batches);
    Ok((rows, bytes))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let concurrency = args.concurrency.max(1);
    let flightsql = !args.no_adbc;

    // Server under test.
    let (_handle, uri) = match &args.uri {
        Some(u) => (None, u.clone()),
        None => {
            let (uri, handle) = ladybug_adbc::serve_ephemeral(":memory:", true).await?;
            (Some(handle), uri)
        }
    };
    let mode = if flightsql {
        "ADBC FlightSQL"
    } else {
        "plain Flight"
    };
    println!("🎯 target: {uri} [{mode}], concurrency={concurrency}");

    let selected: Vec<(String, String)> = if args.query == "all" {
        demo_queries()
            .into_iter()
            .map(|(l, q)| (l.to_string(), q.to_string()))
            .collect()
    } else {
        let q = demo_queries()
            .into_iter()
            .find(|(l, _)| *l == args.query)
            .map(|(_, q)| q.to_string())
            .unwrap_or_else(|| args.query.clone());
        vec![(args.query.clone(), q)]
    };

    let mut rows_out = Vec::new();
    for (label, cypher) in &selected {
        // Warmup (untimed) + one probe for per-query row/byte counts.
        // Each worker holds one persistent HTTP/2 connection so the timed
        // loop measures server throughput, not TCP/TLS handshake cost.
        let mut warm = LadybugClient::connect(&uri).await?;
        for _ in 0..args.warmup {
            run_once(&mut warm, cypher, flightsql).await?;
        }
        let (rows_per_q, bytes_per_q) = run_once(&mut warm, cypher, flightsql).await?;
        drop(warm);

        let iters = args.iterations;
        let per_task = iters.div_ceil(concurrency);
        let start = Instant::now();
        let mut tasks = Vec::new();
        for t in 0..concurrency {
            let n = (per_task * (t + 1)).min(iters) - per_task * t;
            if n == 0 {
                continue;
            }
            let uri = uri.clone();
            let cypher = cypher.clone();
            tasks.push(tokio::spawn(async move {
                let mut client = LadybugClient::connect(&uri).await?;
                for _ in 0..n {
                    run_once(&mut client, &cypher, flightsql).await?;
                }
                Ok::<_, anyhow::Error>(())
            }));
        }
        for t in tasks {
            t.await??;
        }
        let secs = start.elapsed().as_secs_f64();
        rows_out.push(Row {
            label: label.clone(),
            iters,
            secs,
            rows_per_q,
            bytes_per_q,
        });
    }

    println!(
        "\n{:<22} {:>8} {:>10} {:>10} {:>12} {:>12} {:>10}",
        "query", "iters", "qps", "rows/s", "MiB/s", "rows/q", "ms/q"
    );
    let mut tot_q = 0usize;
    let mut tot_rows = 0usize;
    let mut tot_bytes = 0usize;
    let mut tot_secs = 0f64;
    for r in &rows_out {
        println!(
            "{:<22} {:>8} {:>10.1} {:>10.0} {:>12.2} {:>12} {:>10.2}",
            r.label,
            r.iters,
            r.qps(),
            r.rows_per_s(),
            r.mib_per_s(),
            r.rows_per_q,
            r.secs * 1000.0 / r.iters as f64
        );
        tot_q += r.iters;
        tot_rows += r.rows_per_q * r.iters;
        tot_bytes += r.bytes_per_q * r.iters;
        tot_secs += r.secs;
    }
    println!(
        "{:<22} {:>8} {:>10.1} {:>10.0} {:>12.2}",
        "TOTAL",
        tot_q,
        tot_q as f64 / tot_secs,
        tot_rows as f64 / tot_secs,
        tot_bytes as f64 / tot_secs / (1024.0 * 1024.0)
    );
    Ok(())
}
