//! Arrow Flight / ADBC columnar interface for LadybugDB.
//!
//! Rust port of `ladybug-adbc-python`: serves LadybugDB Cypher results as
//! native Arrow columnar batches over gRPC (Arrow Flight), queryable with the
//! standard columnar.tech-style call pattern — no row-by-row JSON.
//!
//! # SQL vs Cypher note
//!
//! The ADBC Flight SQL driver is *typically* tied to SQL (its wire message is
//! `CommandStatementQuery { query }`), but the `query` field is just a string.
//! This crate treats that string as **Cypher** (falling back to Cypher for
//! anything that is not a `CALL show_tables()` meta query), so the exact same
//! call pattern works for a graph database. No SQL engine required.

use std::sync::Arc;

use arrow::array::{RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

pub mod client;
pub mod server;

pub use client::{query_cypher_flight, query_cypher_flightsql, LadybugClient};
pub use server::{serve_ephemeral, LadybugFlightServer};

/// Record-batch chunk size for LadybugDB's Arrow result collector.
pub const CHUNK_SIZE: usize = 2048;

/// Demo dataset. Mirrors `build_demo_graph` in the Python package (and
/// `app.build_demo_graph`) so REST and Flight return the same data.
pub fn demo_queries() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "all_users",
            "MATCH (u:User) RETURN u.name, u.age ORDER BY u.id",
        ),
        (
            "follows",
            "MATCH (a:User)-[f:Follows]->(b:User) \
             RETURN a.name AS follower, b.name AS followed, f.since ORDER BY f.since",
        ),
        (
            "lives_in",
            "MATCH (u:User)-[:LivesIn]->(c:City) RETURN u.name, c.name AS city ORDER BY u.name",
        ),
        (
            "sf_users",
            "MATCH (u:User)-[:LivesIn]->(c:City {name: 'San Francisco'}) RETURN u.name, u.age",
        ),
        (
            "two_hop_from_alice",
            "MATCH (a:User {name: 'Alice'})-[:Follows]->(b:User)-[:Follows]->(c:User) \
             RETURN a.name AS start, b.name AS via, c.name AS reached",
        ),
        (
            "same_city_follows",
            "MATCH (a:User)-[:Follows]->(b:User), \
             (a)-[:LivesIn]->(c:City)<-[:LivesIn]-(b) \
             RETURN a.name AS user, b.name AS follows, c.name AS shared_city",
        ),
    ]
}

/// Create schema + rows. Idempotent-ish: ignores 'already exists' errors,
/// and duplicate inserts (mirrors the Python `build_demo_graph`).
pub fn build_demo_graph(conn: &lbug::Connection) -> anyhow::Result<()> {
    for stmt in [
        "CREATE NODE TABLE User (id INT64, name STRING, age INT64, PRIMARY KEY(id))",
        "CREATE NODE TABLE City (id INT64, name STRING, population INT64, PRIMARY KEY(id))",
        "CREATE REL TABLE Follows (FROM User TO User, since INT64)",
        "CREATE REL TABLE LivesIn (FROM User TO City)",
    ] {
        match conn.query(stmt) {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if !(msg.contains("already exists") || msg.contains("duplicate")) {
                    return Err(e.into());
                }
            }
        }
    }
    for (uid, name, age) in [(1, "Alice", 30), (2, "Bob", 25), (3, "Carol", 35), (4, "Dave", 28), (5, "Eve", 22)] {
        let _ = conn.query(&format!(
            "CREATE (u:User {{id: {uid}, name: '{name}', age: {age}}})"
        ));
    }
    for (cid, name, pop) in [
        (1, "San Francisco", 874961),
        (2, "New York", 8336817),
        (3, "Chicago", 2693976),
    ] {
        let _ = conn.query(&format!(
            "CREATE (c:City {{id: {cid}, name: '{name}', population: {pop}}})"
        ));
    }
    for (src, dst, since) in [
        (1, 2, 2020),
        (1, 3, 2021),
        (2, 4, 2022),
        (3, 5, 2023),
        (4, 1, 2021),
        (5, 2, 2024),
    ] {
        let _ = conn.query(&format!(
            "MATCH (a:User {{id:{src}}}), (b:User {{id:{dst}}}) \
             CREATE (a)-[:Follows {{since: {since}}}]->(b)"
        ));
    }
    for (uid, cid) in [(1, 1), (2, 2), (3, 1), (4, 3), (5, 2)] {
        let _ = conn.query(&format!(
            "MATCH (u:User {{id:{uid}}}), (c:City {{id:{cid}}}) \
             CREATE (u)-[:LivesIn]->(c)"
        ));
    }
    Ok(())
}

/// Execute Cypher against LadybugDB, returning native Arrow (schema + batches).
///
/// Mirrors `LadybugFlightServer.execute_cypher`: DDL / empty results collapse
/// to a single-row `status` table, everything else keeps native Arrow types.
pub fn execute_cypher(
    conn: &lbug::Connection,
    cypher: &str,
    chunk_size: usize,
) -> anyhow::Result<(SchemaRef, Vec<RecordBatch>)> {
    let cypher = cypher.trim();
    if cypher.is_empty() {
        anyhow::bail!("empty query");
    }
    let mut result = conn
        .query_as_arrow(cypher, chunk_size)
        .map_err(|e| anyhow::anyhow!("ladybug query failed: {e}"))?;

    // Peek at metadata before consuming: DDL has no columns -> status table.
    if result.get_num_columns() == 0 {
        return Ok(status_table());
    }
    let names = result.get_column_names();
    let mut batches: Vec<RecordBatch> = result
        .iter_arrow(chunk_size)
        .map_err(|e| anyhow::anyhow!("ladybug arrow export failed: {e}"))?
        .collect();
    // De-duplicate the schema message some builds repeat on empty streams.
    if batches.is_empty() {
        // Zero-row result: preserve column names with (empty) Utf8 columns.
        let fields: Vec<Field> = names
            .iter()
            .map(|n| Field::new(n, DataType::Utf8, true))
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new(fields));
        let cols: Vec<Arc<dyn arrow::array::Array>> = names
            .iter()
            .map(|_| Arc::new(StringArray::from(Vec::<&str>::new())) as Arc<dyn arrow::array::Array>)
            .collect();
        batches.push(
            RecordBatch::try_new(schema.clone(), cols)
                .map_err(|e| anyhow::anyhow!("empty batch build failed: {e}"))?,
        );
        return Ok((schema, batches));
    }
    let schema = batches[0].schema();
    Ok((schema, batches))
}

fn status_table() -> (SchemaRef, Vec<RecordBatch>) {
    let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new(
        "status",
        DataType::Utf8,
        false,
    )]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["ok"])) as Arc<dyn arrow::array::Array>],
    )
    .expect("status table build");
    (schema, vec![batch])
}

/// (rows, cols, bytes) for a materialised Arrow result.
pub fn table_stats(schema: &Schema, batches: &[RecordBatch]) -> (usize, usize, usize) {
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let bytes: usize = batches.iter().map(RecordBatch::get_array_memory_size).sum();
    (rows, schema.fields().len(), bytes)
}

// ---------------------------------------------------------------------------
// Minimal Flight SQL protobuf codec (no prost dependency on the wire types).
//
// Mirrors the Python `ladybug_flight_server` codec: only varint +
// length-delimited fields are needed for the messages used here:
//   Any, CommandStatementQuery{1:string query},
//   CommandPreparedStatementQuery{1:bytes handle},
//   TicketStatementQuery{1:bytes handle},
//   ActionCreatePreparedStatementRequest{1:string query},
//   ActionCreatePreparedStatementResult{1:bytes handle, 2:bytes dataset_schema},
//   ActionClosePreparedStatementRequest{1:bytes handle}
// ---------------------------------------------------------------------------

const SQL_PKG: &str = "arrow.flight.protocol.sql";

/// Keywords that mark a string as a query (mirrors Python `_QUERY_HINT`).
const QUERY_KEYWORDS: &[&str] = &[
    "MATCH", "RETURN", "CREATE", "CALL", "UNWIND", "WITH", "SELECT", "SHOW", "DESCRIBE",
    "EXPLAIN",
];

fn looks_like_query(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| {
            let up = tok.to_ascii_uppercase();
            QUERY_KEYWORDS.iter().any(|k| *k == up)
        })
}

pub fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let bits = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            out.push(bits | 0x80);
        } else {
            out.push(bits);
            break;
        }
    }
    out
}

pub fn encode_ld(field: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = encode_varint(((field << 3) | 2) as u64);
    out.extend_from_slice(&encode_varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

/// Parse top-level protobuf fields -> (field_no, wire_type, raw_payload).
pub fn decode_fields(blob: &[u8]) -> Vec<(u32, u8, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut i = 0;
    let n = blob.len();
    while i < n {
        // tag varint
        let mut tag: u64 = 0;
        let mut shift = 0;
        let mut j = i;
        let mut ok = false;
        while j < n {
            let b = blob[j];
            j += 1;
            tag |= ((b & 0x7F) as u64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                ok = true;
                break;
            }
            if shift > 64 {
                break;
            }
        }
        if !ok || j > i + 10 {
            break;
        }
        i = j;
        let wire_type = (tag & 0x7) as u8;
        let field_no = (tag >> 3) as u32;
        match wire_type {
            2 => {
                let mut length: u64 = 0;
                let mut shift = 0;
                let mut j = i;
                let mut ok = false;
                while j < n {
                    let b = blob[j];
                    j += 1;
                    length |= ((b & 0x7F) as u64) << shift;
                    shift += 7;
                    if b & 0x80 == 0 {
                        ok = true;
                        break;
                    }
                    if shift > 64 {
                        break;
                    }
                }
                if !ok || length > 50_000_000 || j + length as usize > n {
                    break;
                }
                i = j;
                fields.push((field_no, wire_type, blob[i..i + length as usize].to_vec()));
                i += length as usize;
            }
            0 => {
                let mut j = i;
                while j < n {
                    let b = blob[j];
                    j += 1;
                    if b & 0x80 == 0 {
                        break;
                    }
                }
                fields.push((field_no, wire_type, blob[i..j].to_vec()));
                i = j;
            }
            1 => {
                if i + 8 > n {
                    break;
                }
                fields.push((field_no, wire_type, blob[i..i + 8].to_vec()));
                i += 8;
            }
            5 => {
                if i + 4 > n {
                    break;
                }
                fields.push((field_no, wire_type, blob[i..i + 4].to_vec()));
                i += 4;
            }
            _ => break,
        }
    }
    fields
}

pub fn encode_any(type_name: &str, value: &[u8]) -> Vec<u8> {
    let type_url = format!("type.googleapis.com/{SQL_PKG}.{type_name}");
    let mut out = encode_ld(1, type_url.as_bytes());
    out.extend_from_slice(&encode_ld(2, value));
    out
}

pub fn decode_any(blob: &[u8]) -> (String, Vec<u8>) {
    let mut type_url = String::new();
    let mut value = Vec::new();
    for (field_no, wire, payload) in decode_fields(blob) {
        if field_no == 1 && wire == 2 {
            if let Ok(s) = String::from_utf8(payload.clone()) {
                type_url = s;
            }
        } else if field_no == 2 && wire == 2 {
            value = payload;
        }
    }
    (type_url, value)
}

fn get_str(fields: &[(u32, u8, Vec<u8>)], field_no: u32) -> String {
    for (f, w, p) in fields {
        if *f == field_no && *w == 2 {
            return String::from_utf8_lossy(p).into_owned();
        }
    }
    String::new()
}

fn get_bytes(fields: &[(u32, u8, Vec<u8>)], field_no: u32) -> Vec<u8> {
    for (f, w, p) in fields {
        if *f == field_no && *w == 2 {
            return p.clone();
        }
    }
    Vec::new()
}

/// Best-effort query string from a Flight descriptor/ticket payload.
pub fn extract_query(cmd: &[u8]) -> String {
    // 1. Proper Flight SQL Any(...) wrapper.
    let (type_url, value) = decode_any(cmd);
    if !value.is_empty() && type_url.contains("CommandStatementQuery") {
        let q = get_str(&decode_fields(&value), 1);
        if !q.trim().is_empty() {
            return q.trim().to_string();
        }
    }
    if !value.is_empty() && type_url.contains("ActionCreatePreparedStatementRequest") {
        let q = get_str(&decode_fields(&value), 1);
        if !q.trim().is_empty() {
            return q.trim().to_string();
        }
    }
    // 2. Raw UTF-8 (plain FlightDescriptor.for_command(cypher)): prefer it
    // verbatim -- parsing raw Cypher as protobuf yields spurious fragments.
    let raw = String::from_utf8_lossy(cmd).trim().to_string();
    if !raw.is_empty() && looks_like_query(&raw) {
        return raw;
    }
    // 3. Bare CommandStatementQuery (no Any wrapper).
    let q = get_str(&decode_fields(cmd), 1);
    if !q.trim().is_empty()
        && looks_like_query(&q)
        && !q.contains("type.googleapis.com")
    {
        return q.trim().to_string();
    }
    // 4. Scan every nested string for something query-like.
    let mut best = String::new();
    for (_, wire, payload) in decode_fields(cmd) {
        if wire != 2 {
            continue;
        }
        let Ok(s) = String::from_utf8(payload) else {
            continue;
        };
        let s = s.trim().to_string();
        if s.contains("type.googleapis.com") || s.contains("arrow.flight") {
            continue;
        }
        if looks_like_query(&s) && s.len() > best.len() {
            best = s;
        }
    }
    if !best.is_empty() {
        return best;
    }
    // 5. Whatever raw text we got (or lossy decode of binary).
    raw
}

/// Ticket -> prepared-statement handle (TicketStatementQuery{1} or raw).
pub fn decode_ticket_handle(ticket: &[u8]) -> Vec<u8> {
    for (f, w, p) in decode_fields(ticket) {
        if f == 1 && w == 2 && !p.is_empty() {
            return p;
        }
    }
    let (_, value) = decode_any(ticket);
    if !value.is_empty() {
        for (f, w, p) in decode_fields(&value) {
            if f == 1 && w == 2 && !p.is_empty() {
                return p;
            }
        }
    }
    ticket.to_vec()
}

// -- Typed message builders / parsers (used by server + FlightSQL client) ---

pub fn any_command_statement_query(query: &str) -> Vec<u8> {
    encode_any("CommandStatementQuery", &encode_ld(1, query.as_bytes()))
}

pub fn any_command_prepared_statement_query(handle: &[u8]) -> Vec<u8> {
    encode_any(
        "CommandPreparedStatementQuery",
        &encode_ld(1, handle),
    )
}

pub fn ticket_statement_query(handle: &[u8]) -> Vec<u8> {
    encode_ld(1, handle)
}

pub fn action_create_prepared_statement_request(query: &str) -> Vec<u8> {
    encode_any(
        "ActionCreatePreparedStatementRequest",
        &encode_ld(1, query.as_bytes()),
    )
}

/// Parse a CreatePreparedStatement action body (Any-wrapped or raw).
pub fn parse_create_prepared_statement_request(body: &[u8]) -> String {
    let (type_url, value) = decode_any(body);
    let query = if type_url.contains("CreatePreparedStatement") && !value.is_empty() {
        get_str(&decode_fields(&value), 1)
    } else {
        String::new()
    };
    if !query.is_empty() {
        return query;
    }
    let bare = get_str(&decode_fields(body), 1);
    if !bare.is_empty() {
        return bare;
    }
    extract_query(body)
}

/// Build the CreatePreparedStatement result (Any-wrapped, mirrors Python).
pub fn encode_create_prepared_statement_result(handle: &[u8], dataset_schema_ipc: &[u8]) -> Vec<u8> {
    let mut msg = encode_ld(1, handle);
    msg.extend_from_slice(&encode_ld(2, dataset_schema_ipc));
    encode_any("ActionCreatePreparedStatementResult", &msg)
}

/// Parse a CreatePreparedStatement result -> (handle, dataset_schema_ipc).
pub fn parse_create_prepared_statement_result(body: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (type_url, value) = decode_any(body);
    let inner = if !value.is_empty()
        && (type_url.contains("CreatePreparedStatementResult") || !type_url.is_empty())
    {
        value
    } else {
        body.to_vec()
    };
    let fields = decode_fields(&inner);
    (get_bytes(&fields, 1), get_bytes(&fields, 2))
}

pub fn action_close_prepared_statement_request(handle: &[u8]) -> Vec<u8> {
    encode_ld(1, handle)
}

/// Parse a ClosePreparedStatement action body -> handle.
pub fn parse_close_prepared_statement_request(body: &[u8]) -> Vec<u8> {
    let (_, value) = decode_any(body);
    let fields = decode_fields(if value.is_empty() { body } else { &value });
    let handle = get_bytes(&fields, 1);
    if !handle.is_empty() {
        return handle;
    }
    decode_ticket_handle(body)
}

#[cfg(test)]
mod codec_tests {
    use super::*;

    #[test]
    fn raw_and_protobuf_extract() {
        assert_eq!(
            extract_query(b"MATCH (u:User) RETURN u.name"),
            "MATCH (u:User) RETURN u.name"
        );
        let mut inner = encode_ld(1, b"MATCH (u:User) RETURN u.name");
        let _ = &mut inner;
        let blob = {
            let mut b = encode_ld(
                1,
                b"type.googleapis.com/arrow.flight.protocol.sql.CommandStatementQuery",
            );
            b.extend_from_slice(&encode_ld(2, &inner));
            b
        };
        assert_eq!(
            extract_query(&blob),
            "MATCH (u:User) RETURN u.name"
        );
    }
}
