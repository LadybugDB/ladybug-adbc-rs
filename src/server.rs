//! Arrow Flight server executing Cypher against LadybugDB.
//!
//! Mirrors `LadybugFlightServer(FlightServerBase)` from the Python package:
//! plain `FlightService` plus the Flight SQL prepared-statement handshake
//! (`CreatePreparedStatement` action -> `GetFlightInfo` with
//! `CommandPreparedStatementQuery` -> `DoGet` with `TicketStatementQuery`),
//! plus a plain-Flight fallback (`FlightDescriptor::new_cmd(cypher)`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::flight_descriptor::DescriptorType;
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use tonic::{Request, Response, Status, Streaming};

use crate::{
    action_close_prepared_statement_request, decode_ticket_handle,
    encode_create_prepared_statement_result, extract_query, parse_close_prepared_statement_request,
    parse_create_prepared_statement_request, ticket_statement_query, CHUNK_SIZE,
};

/// Cap on cached prepared statements (mirrors Python's 512-entry dict).
const MAX_PREPARED: usize = 512;

struct PreparedEntry {
    query: String,
}

/// Bounded prepared-statement cache with oldest-first eviction.
///
/// Insertion order is tracked explicitly: unlike Python's `dict` (which is
/// insertion-ordered, so `pop(next(iter()))` always drops the *oldest*
/// entry), Rust's `HashMap` iterates arbitrarily — evicting an arbitrary
/// entry could drop the handle whose ticket is still in flight.
struct PreparedCache {
    map: HashMap<String, PreparedEntry>,
    order: VecDeque<String>,
}

impl PreparedCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, key: String, entry: PreparedEntry) {
        // Handles are UUIDs: always fresh keys, appended at the back, so
        // oldest-first eviction can never drop the just-inserted handle.
        self.order.push_back(key.clone());
        self.map.insert(key, entry);
        while self.map.len() > MAX_PREPARED {
            match self.order.pop_front() {
                Some(old) => {
                    self.map.remove(&old);
                }
                None => break,
            }
        }
    }

    fn get(&self, key: &str) -> Option<&PreparedEntry> {
        self.map.get(key)
    }

    fn remove(&mut self, key: &str) {
        self.map.remove(key);
        // `order` keeps a stale marker; harmless (bounded by insert count,
        // and `remove` of a missing key is a no-op during eviction).
        if self.map.len() * 2 < self.order.len() {
            self.order.retain(|k| self.map.contains_key(k));
        }
    }
}

/// Arrow Flight server executing Cypher against LadybugDB.
///
/// Holds an `Arc<Database>` and mints a fresh `Connection` per RPC
/// (connections are `Send + Sync`; LadybugDB supports concurrent readers).
/// Prepared statements cache the query string only and re-execute on every
/// fetch, so repeated fetches always see fresh data.
#[derive(Clone)]
pub struct LadybugFlightServer {
    db: Arc<lbug::Database>,
    prepared: Arc<Mutex<PreparedCache>>,
    location: String,
}

impl LadybugFlightServer {
    pub fn new(db_path: &str, preload_demo: bool, location: String) -> anyhow::Result<Self> {
        let db = if db_path == ":memory:" {
            lbug::Database::in_memory(lbug::SystemConfig::default())?
        } else {
            lbug::Database::new(db_path, lbug::SystemConfig::default())?
        };
        if preload_demo {
            let conn = lbug::Connection::new(&db)?;
            crate::build_demo_graph(&conn)?;
        }
        Ok(Self {
            db: Arc::new(db),
            prepared: Arc::new(Mutex::new(PreparedCache::new())),
            location,
        })
    }

    /// Blocking Cypher execution on the rayon-free blocking pool.
    async fn execute(&self, cypher: &str) -> Result<(SchemaRef, Vec<RecordBatch>), String> {
        let db = Arc::clone(&self.db);
        let cypher = cypher.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = lbug::Connection::new(&db).map_err(|e| e.to_string())?;
            crate::execute_cypher(&conn, &cypher, CHUNK_SIZE).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn store(&self, query: String) -> Vec<u8> {
        let handle = uuid::Uuid::new_v4().simple().to_string().into_bytes();
        let key = String::from_utf8_lossy(&handle).into_owned();
        self.prepared
            .lock()
            .expect("prepared lock")
            .insert(key, PreparedEntry { query });
        handle
    }

    fn lookup(&self, handle: &[u8]) -> Option<String> {
        let key = String::from_utf8_lossy(handle).into_owned();
        self.prepared
            .lock()
            .expect("prepared lock")
            .get(&key)
            .map(|e| e.query.clone())
    }

    fn flight_info_for(
        &self,
        schema: &SchemaRef,
        rows: i64,
        bytes: i64,
        handle: &[u8],
        descriptor: Option<FlightDescriptor>,
    ) -> Result<FlightInfo, Status> {
        let ticket = Ticket::new(ticket_statement_query(handle));
        let endpoint = FlightEndpoint::new()
            .with_ticket(ticket)
            .with_location(self.location.clone());
        let mut info = FlightInfo::new()
            .try_with_schema(schema)
            .map_err(|e| Status::internal(format!("schema encode failed: {e}")))?
            .with_endpoint(endpoint)
            .with_total_records(rows)
            .with_total_bytes(bytes);
        if let Some(d) = descriptor {
            info = info.with_descriptor(d);
        }
        Ok(info)
    }

    fn schema_ipc(&self, schema: &SchemaRef) -> Result<Vec<u8>, Status> {
        Ok(FlightInfo::new()
            .try_with_schema(schema)
            .map_err(|e| Status::internal(format!("schema encode failed: {e}")))?
            .schema
            .to_vec())
    }

    /// Descriptor -> (cypher, prepared_handle or None).
    fn resolve_descriptor_query(
        &self,
        descriptor: &FlightDescriptor,
    ) -> Result<(String, Option<Vec<u8>>), Status> {
        let blob: Vec<u8> = if descriptor.r#type == DescriptorType::Path as i32 {
            descriptor.path.join("\n").into_bytes()
        } else {
            descriptor.cmd.to_vec()
        };
        let (type_url, value) = crate::decode_any(&blob);
        if type_url.contains("CommandPreparedStatementQuery") {
            let handle = {
                let fields = crate::decode_fields(&value);
                let mut h = Vec::new();
                for (f, w, p) in &fields {
                    if *f == 1 && *w == 2 {
                        h = p.clone();
                        break;
                    }
                }
                h
            };
            match self.lookup(&handle) {
                Some(query) => return Ok((query, Some(handle))),
                None => {
                    return Err(Status::not_found("unknown prepared statement handle"));
                }
            }
        }
        if type_url.contains("CommandStatementQuery") && !value.is_empty() {
            let fields = crate::decode_fields(&value);
            let mut q = String::new();
            for (f, w, p) in &fields {
                if *f == 1 && *w == 2 {
                    q = String::from_utf8_lossy(p).into_owned();
                    break;
                }
            }
            return Ok((q, None));
        }
        Ok((extract_query(&blob), None))
    }
}

fn bad_query(e: String) -> Status {
    if e.contains("empty query") {
        Status::invalid_argument(e)
    } else {
        Status::internal(e)
    }
}

#[tonic::async_trait]
impl FlightService for LadybugFlightServer {
    type HandshakeStream = BoxStream<'static, Result<HandshakeResponse, Status>>;
    type ListFlightsStream = BoxStream<'static, Result<FlightInfo, Status>>;
    type DoGetStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoPutStream = BoxStream<'static, Result<PutResult, Status>>;
    type DoActionStream = BoxStream<'static, Result<arrow_flight::Result, Status>>;
    type ListActionsStream = BoxStream<'static, Result<ActionType, Status>>;
    type DoExchangeStream = BoxStream<'static, Result<FlightData, Status>>;

    async fn handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        Ok(Response::new(futures::stream::empty().boxed()))
    }

    async fn list_flights(
        &self,
        _request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        let mut infos = Vec::new();
        for (label, cypher) in crate::demo_queries() {
            let (schema, batches) = self.execute(cypher).await.map_err(bad_query)?;
            let (rows, _, bytes) = crate::table_stats(&schema, &batches);
            let handle = self.store(cypher.to_string());
            let descriptor = FlightDescriptor::new_path(vec!["demo".to_string(), label.to_string()]);
            infos.push(Ok(self.flight_info_for(
                &schema,
                rows as i64,
                bytes as i64,
                &handle,
                Some(descriptor),
            )?));
        }
        Ok(Response::new(futures::stream::iter(infos).boxed()))
    }

    async fn get_flight_info(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let descriptor = request.into_inner();
        let (cypher, handle) = self.resolve_descriptor_query(&descriptor)?;
        let (schema, batches) = self.execute(&cypher).await.map_err(bad_query)?;
        let (rows, _, bytes) = crate::table_stats(&schema, &batches);
        let handle = handle.unwrap_or_else(|| self.store(cypher));
        let info = self.flight_info_for(
            &schema,
            rows as i64,
            bytes as i64,
            &handle,
            Some(descriptor),
        )?;
        Ok(Response::new(info))
    }

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented("poll_flight_info not supported"))
    }

    async fn get_schema(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        let descriptor = request.into_inner();
        let (cypher, _handle) = self.resolve_descriptor_query(&descriptor)?;
        let (schema, _batches) = self.execute(&cypher).await.map_err(bad_query)?;
        Ok(Response::new(SchemaResult {
            schema: self.schema_ipc(&schema)?.into(),
        }))
    }

    async fn do_get(&self, request: Request<Ticket>) -> Result<Response<Self::DoGetStream>, Status> {
        let raw = request.into_inner().ticket.to_vec();
        let handle = decode_ticket_handle(&raw);
        let cypher = match self.lookup(&handle) {
            Some(q) => q,
            None => extract_query(&raw),
        };
        if cypher.trim().is_empty() {
            return Err(Status::invalid_argument("empty query"));
        }
        let (schema, batches) = self.execute(&cypher).await.map_err(bad_query)?;
        let encoder = arrow_flight::encode::FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .build(futures::stream::iter(batches.into_iter().map(Ok)));
        let stream = encoder.map_err(|e| Status::internal(e.to_string())).boxed();
        Ok(Response::new(stream))
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        Err(Status::unimplemented("do_put not supported"))
    }

    async fn do_action(&self, request: Request<Action>) -> Result<Response<Self::DoActionStream>, Status> {
        let action = request.into_inner();
        match action.r#type.as_str() {
            "CreatePreparedStatement" => {
                let query = parse_create_prepared_statement_request(&action.body);
                if query.trim().is_empty() {
                    return Err(Status::invalid_argument("empty query"));
                }
                let (schema, _batches) = self.execute(&query).await.map_err(bad_query)?;
                let dataset_schema = self.schema_ipc(&schema)?;
                let handle = self.store(query);
                let body = encode_create_prepared_statement_result(&handle, &dataset_schema);
                let out: Vec<Result<arrow_flight::Result, Status>> =
                    vec![Ok(arrow_flight::Result { body: body.into() })];
                Ok(Response::new(futures::stream::iter(out).boxed()))
            }
            "ClosePreparedStatement" => {
                let handle = parse_close_prepared_statement_request(&action.body);
                // Accept the same encoding our client sends.
                let _ = action_close_prepared_statement_request(&handle);
                let key = String::from_utf8_lossy(&handle).into_owned();
                self.prepared.lock().expect("prepared lock").remove(&key);
                let out: Vec<Result<arrow_flight::Result, Status>> =
                    vec![Ok(arrow_flight::Result { body: vec![].into() })];
                Ok(Response::new(futures::stream::iter(out).boxed()))
            }
            "show_tables" | "schema" | "list_queries" => {
                let text = if action.r#type == "list_queries" {
                    crate::demo_queries()
                        .iter()
                        .map(|(l, q)| format!("{l}: {q}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    match self.execute("CALL show_tables() RETURN *").await {
                        Ok((schema, batches)) => {
                            let (rows, cols, _) = crate::table_stats(&schema, &batches);
                            format!("show_tables: {rows} rows x {cols} cols")
                        }
                        Err(e) => format!("show_tables failed: {e}"),
                    }
                };
                let out: Vec<Result<arrow_flight::Result, Status>> =
                    vec![Ok(arrow_flight::Result { body: text.into_bytes().into() })];
                Ok(Response::new(futures::stream::iter(out).boxed()))
            }
            other => Err(Status::unimplemented(format!("unknown action: {other}"))),
        }
    }

    async fn list_actions(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        let actions = vec![
            Ok(ActionType {
                r#type: "CreatePreparedStatement".to_string(),
                description: "Create a prepared Cypher statement".to_string(),
            }),
            Ok(ActionType {
                r#type: "ClosePreparedStatement".to_string(),
                description: "Close a prepared Cypher statement".to_string(),
            }),
            Ok(ActionType {
                r#type: "show_tables".to_string(),
                description: "CALL show_tables() schema introspection".to_string(),
            }),
            Ok(ActionType {
                r#type: "list_queries".to_string(),
                description: "List demo queries".to_string(),
            }),
        ];
        Ok(Response::new(futures::stream::iter(actions).boxed()))
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("do_exchange not supported"))
    }
}

/// Serve on an ephemeral `127.0.0.1:0` port; returns `(grpc_uri, join_handle)`.
///
/// Shared by tests, benches and the `ladybug-bench` binary.
pub async fn serve_ephemeral(
    db_path: &str,
    preload_demo: bool,
) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let location = format!("grpc://127.0.0.1:{port}");
    let svc = LadybugFlightServer::new(db_path, preload_demo, location.clone())?;
    let stream = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let handle = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(FlightServiceServer::new(svc))
            .serve_with_incoming(stream)
            .await;
    });
    Ok((location, handle))
}
