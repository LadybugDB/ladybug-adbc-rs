//! Container HEALTHCHECK: probe the Flight server with a trivial query.

use ladybug_adbc::query_cypher_flight;

#[tokio::main]
async fn main() {
    let mut host = std::env::var("FLIGHT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    if host == "0.0.0.0" {
        host = "127.0.0.1".to_string();
    }
    let port = std::env::var("FLIGHT_PORT").unwrap_or_else(|_| "50051".to_string());
    let uri = format!("grpc://{host}:{port}");
    let code = match query_cypher_flight(&uri, "RETURN 1 AS ok").await {
        Ok((_, batches)) if batches.iter().map(|b| b.num_rows()).sum::<usize>() >= 1 => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("unhealthy: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
