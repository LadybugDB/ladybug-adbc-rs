//! GDS_PAGE_RANK coverage, ported from
//! https://github.com/LadybugDB/extensions/blob/main/algo/test/test_files/gds_page_rank.test
//!
//! Requires the bundled icebug prebuilt (see `scripts/download-icebug.sh`)
//! for the in-process cross-check, plus the `algo` Ladybug extension loaded
//! at runtime (`INSTALL algo` from `extension.ladybugdb.com`). The test
//! skips gracefully when either is absent so `cargo test` stays green on
//! machines without the bundle or the extension repository reachable.
//!
//! Linux only: the `icebug-analytics` feature pulls in libarrow + libomp
//! and the libnetworkit.so prebuilt (NetworKit). macOS / Windows builds of
//! this crate don't ship those.
//!
//! Two levels, both exercising `INSTALL algo; LOAD algo; CALL GDS_PAGE_RANK`:
//! 1. `gds_page_rank_star` — direct in-process `lbug::Connection`.
//! 2. `gds_page_rank_via_flight` — the same flow over Arrow Flight
//!    (`LadybugFlightServer` loads algo once at startup; the loaded
//!    extension is persistent across the per-RPC connections it mints).

use lbug::{Database, SystemConfig, Value};

fn open_ephemeral() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gds.lbdb");
    let db = Database::new(path.to_str().unwrap(), SystemConfig::default()).unwrap();
    (dir, db)
}

fn exec(conn: &lbug::Connection, q: &str) {
    conn.query(q)
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"));
}

/// Install the algo extension from the official repo
/// (`https://extension.ladybugdb.com`). No-op when already installed.
/// Returns false when the download fails (e.g. offline). Never panics.
fn install_algo_extension(conn: &lbug::Connection) -> bool {
    match conn.query("INSTALL algo") {
        Ok(_) => true,
        Err(e) => {
            eprintln!("INSTALL algo failed: {e}");
            false
        }
    }
}

/// Load the algo extension on this connection. Returns true when
/// `GDS_*` is callable from Cypher. Never panics.
///
/// Resolution order: `LOAD algo` (already installed), otherwise
/// `INSTALL algo` from the official repo followed by `LOAD algo`.
fn ensure_algo_extension(conn: &lbug::Connection) -> bool {
    match conn.query("LOAD algo") {
        Ok(_) => return true,
        Err(e) => eprintln!("LOAD algo (already installed?) failed: {e}"),
    }
    if install_algo_extension(conn) {
        match conn.query("LOAD algo") {
            Ok(_) => return true,
            Err(e) => eprintln!("LOAD algo (after INSTALL) failed: {e}"),
        }
    }
    false
}

/// (id, rank) rows from `CALL GDS_PAGE_RANK(...) RETURN node.id, rank ...`.
fn page_rank_rows(conn: &lbug::Connection, graph: &str) -> Vec<(i64, f64)> {
    let mut out = Vec::new();
    let mut result = conn
        .query(&format!(
            "CALL GDS_PAGE_RANK('{graph}') RETURN node.id, rank ORDER BY rank DESC, node.id"
        ))
        .expect("GDS_PAGE_RANK call failed");
    for row in &mut result {
        let (Value::Int64(id), Value::Double(rank)) = (&row[0], &row[1]) else {
            panic!("unexpected GDS_PAGE_RANK row: {row:?}");
        };
        out.push((*id, *rank));
    }
    out
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn gds_page_rank_star() {
    let (_dir, db) = open_ephemeral();
    let conn = lbug::Connection::new(&db).unwrap();
    // INSTALL first: skip only when the extension repo is unreachable
    // (offline). A successful install followed by a failed LOAD is a real
    // failure, so assert instead of skipping.
    if !install_algo_extension(&conn) {
        eprintln!("SKIP gds_page_rank_star: cannot INSTALL algo (offline?)");
        return;
    }
    assert!(
        ensure_algo_extension(&conn),
        "algo installed but LOAD algo failed"
    );

    // --- GDSPageRankStar setup: hub node 0 with 3 leaves pointing at it ---
    exec(&conn, "CREATE NODE TABLE N(id INT64 PRIMARY KEY)");
    exec(&conn, "CREATE REL TABLE E(FROM N TO N)");
    exec(
        &conn,
        "CREATE (a:N{id:0}), (b:N{id:1}), (c:N{id:2}), (d:N{id:3})",
    );
    for leaf in [1, 2, 3] {
        exec(
            &conn,
            &format!("MATCH (x:N{{id:{leaf}}}), (y:N{{id:0}}) CREATE (x)-[:E]->(y)"),
        );
    }

    exec(&conn, "CALL PROJECT_GRAPH('G', ['N'], ['E'])");
    let rows = page_rank_rows(&conn, "G");
    assert_eq!(rows.len(), 4);
    // Hub (id 0) ranks highest; scores are a sum-to-1 distribution.
    assert_eq!(rows[0].0, 0);
    assert!(approx(rows[0].1, 0.479_730), "hub rank {}", rows[0].1);
    for (_, rank) in rows.iter().skip(1) {
        assert!(approx(*rank, 0.173_423), "leaf rank {rank}");
    }
    let total: f64 = rows.iter().map(|(_, r)| r).sum();
    assert!(approx(total, 1.0), "sum-to-1 distribution, got {total}");

    // Cross-check: the in-process icebug PageRank agrees on the ORDERING —
    // hub first, leaves tied. (Exact scores differ: sink/damping conventions
    // vary between implementations; the GDS values above are authoritative.)
    #[cfg(feature = "icebug-analytics")]
    {
        use std::collections::HashMap;
        // Collect (src_id, dst_id) edges directly from the storage tables.
        let mut edges: Vec<(i64, i64)> = Vec::new();
        let mut result = conn
            .query("MATCH (a:N)-[:E]->(b:N) RETURN a.id, b.id")
            .unwrap();
        for row in &mut result {
            let (Value::Int64(s), Value::Int64(t)) = (&row[0], &row[1]) else {
                panic!("unexpected edge row: {row:?}");
            };
            edges.push((*s, *t));
        }
        let n = 4u64;
        let index: HashMap<i64, u64> = (0..4).map(|i| (i as i64, i as u64)).collect();
        let mut outgoing: Vec<Vec<u64>> = vec![Vec::new(); n as usize];
        let mut incoming: Vec<Vec<u64>> = vec![Vec::new(); n as usize];
        for (s, t) in &edges {
            let (Some(&si), Some(&ti)) = (index.get(s), index.get(t)) else {
                continue;
            };
            if si != ti {
                outgoing[si as usize].push(ti);
                incoming[ti as usize].push(si);
            }
        }
        fn pack(adj: Vec<Vec<u64>>) -> (Vec<u64>, Vec<u64>) {
            let mut indptr = Vec::with_capacity(adj.len() + 1);
            let mut indices = Vec::new();
            for mut neighbors in adj {
                neighbors.sort_unstable();
                neighbors.dedup();
                indptr.push(indices.len() as u64);
                indices.extend(neighbors);
            }
            indptr.push(indices.len() as u64);
            (indices, indptr)
        }
        let (out_idx, out_ind) = pack(outgoing);
        let (in_idx, in_ind) = pack(incoming);
        use arrow56::array::UInt64Array;
        let graph = icebug::GraphR::from_directed_csr(
            n,
            UInt64Array::from(out_idx),
            UInt64Array::from(out_ind),
            UInt64Array::from(in_idx),
            UInt64Array::from(in_ind),
        )
        .expect("icebug GraphR::from_directed_csr");
        let mut pr = icebug::PageRank::new(&graph, 0.85, 1e-9, false).expect("icebug PageRank");
        pr.run().expect("icebug PageRank run");
        let scores = pr.scores().expect("icebug PageRank scores");
        let mut ranked: Vec<(i64, f64)> = index
            .iter()
            .map(|(id, i)| (*id, *scores.get(*i as usize).unwrap_or(&0.0)))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        assert_eq!(ranked[0].0, 0, "hub must rank first, got {ranked:?}");
        assert!(
            ranked.iter().skip(1).all(|(_, r)| approx(*r, ranked[1].1)),
            "leaves tied: {ranked:?}"
        );
    }
}

/// End-to-end over Arrow Flight: the server loads `algo` once at startup
/// (see `LadybugFlightServer::new`), and the loaded extension persists
/// across the per-RPC connections it mints.
///
/// Projected graphs (`PROJECT_GRAPH`) are per-connection state, so a
/// multi-RPC setup cannot share the graph `G` across calls. Instead this
/// test asserts the *extension* resolves on every Flight call: a
/// missing/unloaded algo fails with "not been installed" / "no such
/// function" (offline skip), while a loaded one fails with a graph or
/// data error — proving GDS_PAGE_RANK is callable over the wire.
///
/// The authoritative value check lives in `gds_page_rank_star` (direct
/// in-process connection with single-connection graph sharing).
#[tokio::test]
async fn gds_page_rank_via_flight() {
    use ladybug_adbc::serve_ephemeral;

    let (uri, _handle) = serve_ephemeral(":memory:", false).await.unwrap();

    // Probe: does GDS_PAGE_RANK resolve over Flight? A missing algo
    // fails with "not been installed" — an offline skip. Any OTHER
    // error (e.g. "graph does not exist") proves the extension IS
    // loaded and the function resolves.
    let probe =
        ladybug_adbc::query_cypher_flight(&uri, "CALL GDS_PAGE_RANK('nope') RETURN 1").await;
    match probe {
        Ok(_) => panic!("GDS_PAGE_RANK('nope') should fail (no such graph)"),
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("not been installed")
                || msg.contains("no such function")
                || msg.contains("failed to download")
                || msg.contains("network")
                || msg.contains("connection")
                || msg.contains("resolve")
            {
                eprintln!("SKIP gds_page_rank_via_flight: algo unavailable ({e})");
                return;
            }
            // "graph does not exist" (or similar) = extension loaded,
            // function resolved. This is the success path.
            assert!(
                msg.contains("does not exist")
                    || msg.contains("not found")
                    || msg.contains("no such"),
                "unexpected GDS_PAGE_RANK probe failure (extension may not be loaded): {e}"
            );
            eprintln!("GDS_PAGE_RANK resolves over Flight (expected graph error): {e}");
        }
    }

    // DDL tolerates "already exists" / "duplicate": both Flight flows
    // execute DDL twice (schema probe + DoGet), so the second run must
    // be a no-op. (Mirrors `build_demo_graph`.)
    async fn ddl(uri: &str, cypher: &str) {
        match ladybug_adbc::query_cypher_flight(uri, cypher).await {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if !(msg.contains("already exists") || msg.contains("duplicate")) {
                    panic!("Flight DDL failed: {cypher}\n{e}");
                }
            }
        }
    }

    // Build the star graph over Flight (each call is a separate
    // server-side Connection — all sharing the server's one Database).
    for stmt in [
        "CREATE NODE TABLE N(id INT64 PRIMARY KEY)",
        "CREATE REL TABLE E(FROM N TO N)",
        "CREATE (a:N{id:0}), (b:N{id:1}), (c:N{id:2}), (d:N{id:3})",
    ] {
        ddl(&uri, stmt).await;
    }
    for leaf in [1, 2, 3] {
        ddl(
            &uri,
            &format!("MATCH (x:N{{id:{leaf}}}), (y:N{{id:0}}) CREATE (x)-[:E]->(y)"),
        )
        .await;
    }

    // Sanity: the data landed (proves DDL persists across connections).
    let (_, batches) =
        ladybug_adbc::query_cypher_flightsql(&uri, "MATCH (n:N) RETURN n.id ORDER BY n.id")
            .await
            .expect("MATCH over Flight failed");
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 4, "star graph should have 4 nodes");
}
