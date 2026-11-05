//! INSTALL + LOAD smoke coverage for every dynamically-pulled extension.
//!
//! Extensions are deliberately NOT baked into the image (see Dockerfile):
//! each one is pulled at test time via `INSTALL <name>` from
//! `https://extension.ladybugdb.com`, exactly as a container would. This
//! test needs network and skips gracefully when offline.
//!
//! What it guards: `LOAD` dlopens the extension, which fails loudly when
//! the image is missing a native dependency (e.g. `libpq.so.5` for
//! `pg_client`, or the old `libnetworkit`/otel skew that broke `LOAD algo`
//! in the pre-13.5 image). `INSTALL` alone would not catch that.
//!
//! Staged into the runtime image as `extensions_smoke_check`
//! (`docker run --rm <image> /usr/local/bin/extensions_smoke_check`).

use lbug::{Connection, Database, SystemConfig};

/// Every extension the image must be able to INSTALL + LOAD.
const EXTENSIONS: &[&str] = &[
    "algo",
    "vector",
    "fts",
    "duckdb",
    "pg_client",
    "sqlite",
    "adbc",
];

fn open_ephemeral() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ext_smoke.lbdb");
    let db = Database::new(path.to_str().unwrap(), SystemConfig::default()).unwrap();
    (dir, db)
}

/// INSTALL the extension, downloading it when missing. Returns false when
/// the download fails (offline) so the caller can skip. Never panics.
fn install(conn: &Connection, ext: &str) -> bool {
    match conn.query(&format!("INSTALL {ext}")) {
        Ok(_) => true,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            // Idempotent re-run against a warm extension cache.
            if msg.contains("already installed") {
                return true;
            }
            eprintln!("SKIP {ext}: INSTALL failed (offline?) — {e}");
            false
        }
    }
}

#[test]
fn extensions_install_load() {
    let (_dir, db) = open_ephemeral();
    let conn = Connection::new(&db).unwrap();

    let mut skipped = Vec::new();
    for ext in EXTENSIONS {
        if !install(&conn, ext) {
            skipped.push(*ext);
            continue;
        }
        // LOAD is the assertion: it dlopens the extension and fails when
        // any transitive native library is missing from the image. A
        // successful INSTALL followed by a failed LOAD is a real failure,
        // so panic instead of skipping.
        conn.query(&format!("LOAD {ext}"))
            .unwrap_or_else(|e| panic!("LOAD {ext} failed: {e}"));
        eprintln!("OK: INSTALL + LOAD {ext}");
    }

    if !skipped.is_empty() {
        eprintln!("SKIP extensions_install_load (offline?): {skipped:?}");
    }
    assert!(
        skipped.len() < EXTENSIONS.len(),
        "all extension INSTALLs failed — network unavailable?"
    );
}
