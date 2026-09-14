//! `ladybug-client`: query LadybugDB over Flight / FlightSQL, print Arrow.
//!
//! ```sh
//! ladybug-client
//! ladybug-client --query "MATCH (u:User) RETURN u.name"
//! ladybug-client --demo --no-adbc   # plain Flight instead of the ADBC-equivalent flow
//! ```

use clap::Parser;
use ladybug_adbc::{demo_queries, query_cypher_flight, query_cypher_flightsql, table_stats};

#[derive(Parser, Debug)]
#[command(name = "ladybug-client", about = "LadybugDB ADBC/Flight client demo")]
struct Args {
    #[arg(long, default_value = "grpc://localhost:50051")]
    uri: String,
    #[arg(
        long,
        default_value = "MATCH (u:User) RETURN u.name, u.age ORDER BY u.id"
    )]
    query: String,
    /// Run all demo queries.
    #[arg(long, default_value_t = false)]
    demo: bool,
    /// Use plain Flight, not the ADBC-equivalent prepared-statement flow.
    #[arg(long, default_value_t = false)]
    no_adbc: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mode = if args.no_adbc {
        "plain Flight"
    } else {
        "ADBC FlightSQL"
    };
    let queries: Vec<(String, String)> = if args.demo {
        demo_queries()
            .into_iter()
            .map(|(l, q)| (l.to_string(), q.to_string()))
            .collect()
    } else {
        vec![("query".to_string(), args.query.clone())]
    };

    for (label, cypher) in &queries {
        println!("\n🔌 [{mode}] {label}: {}", cypher.chars().take(100).collect::<String>());
        let (schema, batches) = if args.no_adbc {
            query_cypher_flight(&args.uri, cypher).await?
        } else {
            query_cypher_flightsql(&args.uri, cypher).await?
        };
        let (rows, cols, bytes) = table_stats(&schema, &batches);
        println!("📊 {rows} rows x {cols} cols, {bytes} bytes");
        println!(
            "{}",
            arrow::util::pretty::pretty_format_batches(&batches)?
        );
    }
    Ok(())
}
