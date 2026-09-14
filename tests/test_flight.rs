//! End-to-end: ephemeral Flight server -> FlightSQL (ADBC-equivalent) +
//! plain Flight, same Arrow tables. Mirrors `tests/test_flight.py`.

use arrow::array::RecordBatch;
use arrow::util::pretty::pretty_format_batches;
use ladybug_adbc::{extract_query, serve_ephemeral};

fn batches_to_string(batches: &[RecordBatch]) -> String {
    pretty_format_batches(batches).unwrap().to_string()
}

#[test]
fn test_extract_query_raw_and_protobuf() {
    assert_eq!(
        extract_query(b"MATCH (u:User) RETURN u.name"),
        "MATCH (u:User) RETURN u.name"
    );
    // Proper FlightSQL Any(CommandStatementQuery{query}) encoding.
    let inner = ladybug_adbc::encode_ld(1, b"MATCH (u:User) RETURN u.name");
    let mut blob = ladybug_adbc::encode_ld(
        1,
        b"type.googleapis.com/arrow.flight.protocol.sql.CommandStatementQuery",
    );
    blob.extend_from_slice(&ladybug_adbc::encode_ld(2, &inner));
    assert_eq!(extract_query(&blob), "MATCH (u:User) RETURN u.name");
}

#[tokio::test]
async fn test_plain_flight() {
    let (uri, _handle) = serve_ephemeral(":memory:", true).await.unwrap();
    let (schema, batches) = ladybug_adbc::query_cypher_flight(
        &uri,
        "MATCH (u:User) RETURN u.name, u.age ORDER BY u.id",
    )
    .await
    .unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 5);
    assert_eq!(
        schema
            .fields()
            .iter()
            .map(|f| f.name().as_str())
            .collect::<Vec<_>>(),
        vec!["u.name", "u.age"]
    );
}

#[tokio::test]
async fn test_flightsql_prepared() {
    let (uri, _handle) = serve_ephemeral(":memory:", true).await.unwrap();
    let (_, batches) = ladybug_adbc::query_cypher_flightsql(
        &uri,
        "MATCH (u:User) RETURN u.name, u.age ORDER BY u.id",
    )
    .await
    .unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 5);
}

#[tokio::test]
async fn test_flightsql_matches_plain_flight() {
    let (uri, _handle) = serve_ephemeral(":memory:", true).await.unwrap();
    let cypher = "MATCH (a:User)-[f:Follows]->(b:User) \
                  RETURN a.name AS follower, b.name AS followed, f.since ORDER BY f.since";
    let (_, via_sql) = ladybug_adbc::query_cypher_flightsql(&uri, cypher)
        .await
        .unwrap();
    let (_, via_flight) = ladybug_adbc::query_cypher_flight(&uri, cypher).await.unwrap();
    assert_eq!(batches_to_string(&via_sql), batches_to_string(&via_flight));
}
