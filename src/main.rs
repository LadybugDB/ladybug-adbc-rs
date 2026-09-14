//! `ladybug-flight-server`: Arrow Flight / ADBC server for LadybugDB.
//!
//! ```sh
//! ladybug-flight-server                       # :memory: demo graph on 127.0.0.1:50051
//! ladybug-flight-server --db /tmp/g.lbdb --port 50051
//! ```

use arrow_flight::flight_service_server::FlightServiceServer;
use clap::Parser;
use ladybug_adbc::LadybugFlightServer;

#[derive(Parser, Debug)]
#[command(name = "ladybug-flight-server", about = "LadybugDB Arrow Flight / ADBC server")]
struct Args {
    /// Bind host (env FLIGHT_HOST).
    #[arg(long, default_value = "127.0.0.1", env = "FLIGHT_HOST")]
    host: String,
    /// Bind port (env FLIGHT_PORT).
    #[arg(long, default_value_t = 50051, env = "FLIGHT_PORT")]
    port: u16,
    /// LadybugDB path or :memory: (env LADYBUG_DB).
    #[arg(long, default_value = ":memory:", env = "LADYBUG_DB")]
    db: String,
    /// Skip demo-graph preload.
    #[arg(long, default_value_t = false)]
    no_demo: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let addr = format!("{}:{}", args.host, args.port).parse()?;
    let location = format!("grpc://{}:{}", args.host, args.port);
    let svc = LadybugFlightServer::new(&args.db, !args.no_demo, location.clone())?;

    println!("🚀 Ladybug Flight server on {location} (db={})", args.db);
    println!(
        "   ADBC client: adbc_driver_flightsql.dbapi.connect('{location}') + cursor.execute(<cypher>)"
    );
    println!("   Rust client: ladybug-client --uri {location} --demo");

    tonic::transport::Server::builder()
        .add_service(FlightServiceServer::new(svc))
        .serve(addr)
        .await?;
    Ok(())
}
