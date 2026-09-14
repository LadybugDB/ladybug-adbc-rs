//! `ladybug-bench-million`: transport N rows over Arrow Flight (ADBC) vs
//! row-oriented JSON, same data, same machine.
//!
//! ```sh
//! ladybug-bench-million                          # 1M rows, ephemeral server
//! ladybug-bench-million --rows 2000000 --iterations 3
//! ladybug-bench-million --uri grpc://localhost:50051 --rows 1000000
//! ```
//!
//! The query generates rows server-side (`UNWIND range(...)`), so both legs
//! move exactly the same result set:
//!
//! * **ADBC leg** — timed end-to-end `GetFlightInfo` + `DoGet` round trip
//!   (server execute + Arrow IPC encode + gRPC + client decode).
//! * **JSON leg** — mirrors the Flask REST path (`to_pydict` → `json.dumps`
//!   → client `json.parse`): the fetched Arrow batches are serialized to a
//!   row-oriented JSON string and parsed back. Timed separately as
//!   ser / parse so you can see both halves of the REST tax.

use std::time::Instant;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::DataType;
use clap::Parser;
use ladybug_adbc::{table_stats, LadybugClient};

#[derive(Parser, Debug)]
#[command(
    name = "ladybug-bench-million",
    about = "Transport N rows over ADBC vs JSON"
)]
struct Args {
    /// Flight server URI. If absent, an ephemeral in-process server is started.
    #[arg(long)]
    uri: Option<String>,
    /// Rows to transport.
    #[arg(long, default_value_t = 1_000_000)]
    rows: usize,
    /// Timed ADBC round trips.
    #[arg(long, default_value_t = 3)]
    iterations: usize,
}

fn cell_to_json(col: &dyn Array, row: usize) -> anyhow::Result<serde_json::Value> {
    if col.is_null(row) {
        return Ok(serde_json::Value::Null);
    }
    Ok(match col.data_type() {
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            serde_json::Value::from(
                arrow::array::cast::as_primitive_array::<arrow::datatypes::Int64Type>(col)
                    .value(row),
            )
        }
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
            serde_json::Value::from(
                arrow::array::cast::as_primitive_array::<arrow::datatypes::UInt64Type>(col)
                    .value(row) as u64,
            )
        }
        DataType::Float32 | DataType::Float64 => serde_json::Value::from(
            arrow::array::cast::as_primitive_array::<arrow::datatypes::Float64Type>(col).value(row),
        ),
        DataType::Boolean => serde_json::Value::from(
            col.as_any()
                .downcast_ref::<arrow::array::BooleanArray>()
                .ok_or_else(|| anyhow::anyhow!("bad boolean column"))?
                .value(row),
        ),
        DataType::Utf8 | DataType::LargeUtf8 => serde_json::Value::from(
            arrow::array::cast::as_string_array(col).value(row).to_owned(),
        ),
        other => anyhow::bail!("unsupported type for JSON leg: {other:?}"),
    })
}

/// Row-oriented JSON, one object per row — what the REST path ships.
fn batches_to_json_rows(batches: &[RecordBatch]) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for b in batches {
        let bschema = b.schema();
        let names: Vec<&str> = bschema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect();
        for row in 0..b.num_rows() {
            let mut obj = serde_json::Map::with_capacity(names.len());
            for (c, name) in names.iter().enumerate() {
                obj.insert(
                    (*name).to_string(),
                    cell_to_json(b.column(c).as_ref(), row)?,
                );
            }
            out.push(serde_json::Value::Object(obj));
        }
    }
    Ok(out)
}

/// Arrow IPC decode — the client half of the ADBC codec round trip.
fn ipc_bytes_to_batches(bytes: &[u8]) -> anyhow::Result<Vec<RecordBatch>> {
    let mut reader = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None)?;
    let mut out = Vec::new();
    for b in &mut reader {
        out.push(b?);
    }
    Ok(out)
}

/// Arrow IPC stream bytes — the actual ADBC wire payload.
fn batches_to_ipc_bytes(batches: &[RecordBatch]) -> anyhow::Result<Vec<u8>> {
    if batches.is_empty() {
        anyhow::bail!("no batches to encode");
    }
    let mut buf = Vec::new();
    {
        let mut writer = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &batches[0].schema())?;
        for b in batches {
            writer.write(b)?;
        }
        writer.finish()?;
    }
    Ok(buf)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let n = args.rows;

    let (_handle, uri) = match &args.uri {
        Some(u) => (None, u.clone()),
        None => {
            let (uri, handle) = ladybug_adbc::serve_ephemeral(":memory:", false).await?;
            (Some(handle), uri)
        }
    };

    // Mixed-type rows, generated server-side so both legs move identical data.
    let cypher = format!(
        "UNWIND range(1, {n}) AS i \
         RETURN i AS id, i % 1000 AS bucket, CAST(i AS STRING) AS name"
    );

    println!("🎯 target: {uri}, rows: {n}");
    let mut client = LadybugClient::connect(&uri).await?;

    // ---- ADBC leg: timed end-to-end round trips ----
    let mut adbc_secs = Vec::new();
    let mut last = None;
    for i in 0..args.iterations {
        let t = Instant::now();
        let (schema, batches) = client.query_flight(&cypher).await?;
        let dt = t.elapsed().as_secs_f64();
        let (rows, cols, bytes) = table_stats(&schema, &batches);
        if rows != n {
            anyhow::bail!("expected {n} rows, got {rows}");
        }
        println!(
            "  ADBC  iter {i}: {dt:.3}s  ({:.0} rows/s, {} cols, {:.1} MiB arrow)",
            n as f64 / dt,
            cols,
            bytes as f64 / (1024.0 * 1024.0)
        );
        adbc_secs.push(dt);
        last = Some((schema, batches));
    }
    let (schema, batches) = last.expect("at least one iteration");
    let adbc_best = adbc_secs.iter().cloned().fold(f64::INFINITY, f64::min);

    let (_rows, _cols, _arrow_bytes) = table_stats(&schema, &batches);
    let ipc_bytes = batches_to_ipc_bytes(&batches)?.len();

    // ---- Direct leg: same query, in-process, no transport ----
    // This is the query cost Q embedded in both end-to-end paths.
    // One untimed warmup: the first query in a process pays engine setup.
    let direct_db = lbug::Database::in_memory(lbug::SystemConfig::default())?;
    let direct_conn = lbug::Connection::new(&direct_db)?;
    let _ = ladybug_adbc::execute_cypher(&direct_conn, &cypher, ladybug_adbc::CHUNK_SIZE)?;
    let t = Instant::now();
    let (_dschema, _dbatches) =
        ladybug_adbc::execute_cypher(&direct_conn, &cypher, ladybug_adbc::CHUNK_SIZE)?;
    let direct_dt = t.elapsed().as_secs_f64();
    println!("  direct query (no transport): {direct_dt:.3}s");

    // ---- JSON leg: ser (server `json.dumps`) + parse (client `json.parse`) ----
    // Best of 3: allocator warmth varies run to run.
    let mut best = None;
    for _ in 0..3 {
        let t = Instant::now();
        let json_rows = batches_to_json_rows(&batches)?;
        let convert_dt = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let json_str = serde_json::to_string(&json_rows)?;
        let ser_dt = t.elapsed().as_secs_f64();
        let json_bytes = json_str.len();
        drop(json_rows);
        let t = Instant::now();
        let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
        let parse_dt = t.elapsed().as_secs_f64();
        let parsed_rows = parsed.as_array().map(|a| a.len()).unwrap_or(0);
        assert_eq!(parsed_rows, n, "JSON round trip lost rows");
        let total = convert_dt + ser_dt + parse_dt;
        if best.map(|(b, _, _, _, _): (f64, f64, f64, f64, usize)| total < b).unwrap_or(true) {
            best = Some((total, convert_dt, ser_dt, parse_dt, json_bytes));
        }
    }
    let (_, convert_dt, ser_dt, parse_dt, json_bytes) = best.expect("json leg ran");

    // ---- Arrow IPC codec leg: encode + decode of the same batches ----
    // Same codec the ADBC path uses, minus query + gRPC: the apples-to-apples
    // counterpart to JSON convert+ser+parse above.
    let mut ipc_best = f64::INFINITY;
    let mut ipc_bytes_len = 0;
    for _ in 0..3 {
        let t = Instant::now();
        let ipc = batches_to_ipc_bytes(&batches)?;
        let enc_dt = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let back = ipc_bytes_to_batches(&ipc)?;
        let dec_dt = t.elapsed().as_secs_f64();
        let back_rows: usize = back.iter().map(|b| b.num_rows()).sum();
        assert_eq!(back_rows, n, "IPC round trip lost rows");
        ipc_bytes_len = ipc.len();
        ipc_best = ipc_best.min(enc_dt + dec_dt);
        println!("  IPC codec: enc {enc_dt:.3}s + dec {dec_dt:.3}s");
    }

    let json_total = convert_dt + ser_dt + parse_dt;

    println!("\n{:<12} {:>10} {:>12} {:>12} {:>12}", "", "secs", "rows/s", "MiB", "MiB/s");
    println!(
        "{:<12} {:>10.3} {:>12.0} {:>12.1} {:>12.1}",
        "ADBC",
        adbc_best,
        n as f64 / adbc_best,
        ipc_bytes as f64 / (1024.0 * 1024.0),
        ipc_bytes as f64 / (1024.0 * 1024.0) / adbc_best
    );
    println!(
        "{:<12} {:>10.3} {:>12.0} {:>12.1} {:>12.1}",
        "JSON",
        json_total,
        n as f64 / json_total,
        json_bytes as f64 / (1024.0 * 1024.0),
        json_bytes as f64 / (1024.0 * 1024.0) / json_total
    );
    // REST-like estimate: same query cost + JSON ser/parse (transfer omitted
    // for both legs; on loopback it is ~tens of ms vs seconds of CPU).
    let rest_est = direct_dt + json_total;
    println!(
        "REST-like end-to-end estimate: {rest_est:.3}s (query {direct_dt:.2}s + json {json_total:.2}s)"
    );
    println!(
        "{:<12} {:>10.3} {:>12.0} {:>12.1} {:>12.1}",
        "IPC codec",
        ipc_best,
        n as f64 / ipc_best,
        ipc_bytes_len as f64 / (1024.0 * 1024.0),
        ipc_bytes_len as f64 / (1024.0 * 1024.0) / ipc_best
    );
    println!(
        "\nJSON wire size {:.1}x Arrow IPC; JSON codec {:.1}x IPC codec \
         (convert {:.2}s + ser {:.2}s + parse {:.2}s). \
         REST-like end-to-end {:.1}x ADBC.",
        json_bytes as f64 / ipc_bytes as f64,
        json_total / ipc_best,
        convert_dt,
        ser_dt,
        parse_dt,
        rest_est / adbc_best
    );
    println!(
        "NOTE: GetFlightInfo resolves schema via prepare (no execution); a round \
         trip executes the query once, in DoGet. The codec rows above are the \
         pure transport-format comparison."
    );
    Ok(())
}
