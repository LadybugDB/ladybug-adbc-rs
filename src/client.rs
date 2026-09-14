//! Columnar.tech-style clients for the LadybugDB Flight server.
//!
//! Two flavours, same zero-JSON Arrow result:
//!
//! 1. **Flight SQL prepared statements** ([`query_cypher_flightsql`], the Rust
//!    equivalent of the Python `query_cypher_adbc` recipe): `CreatePreparedStatement`
//!    action -> `GetFlightInfo` with `CommandPreparedStatementQuery` -> `DoGet`
//!    with `TicketStatementQuery` -> `ClosePreparedStatement`. This is exactly
//!    what `adbc_driver_flightsql` does on the wire.
//! 2. **Plain Flight** ([`query_cypher_flight`]): `GetFlightInfo` with
//!    `FlightDescriptor::new_cmd(cypher)` -> `DoGet`.

use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow_flight::decode::FlightRecordBatchStream;
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::{Action, FlightDescriptor, FlightInfo};
use futures::TryStreamExt;
use tonic::transport::Channel;

use crate::{
    action_close_prepared_statement_request, action_create_prepared_statement_request,
    any_command_prepared_statement_query, parse_create_prepared_statement_result,
};

/// `grpc://` -> `http://` so `tonic::transport::Endpoint` accepts the URI.
fn normalize_uri(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("grpc://") {
        format!("http://{rest}")
    } else {
        uri.to_string()
    }
}

async fn connect(uri: &str) -> anyhow::Result<FlightServiceClient<Channel>> {
    Ok(FlightServiceClient::connect(normalize_uri(uri)).await?)
}

/// Persistent client holding one HTTP/2 connection.
///
/// Use this for throughput measurements: the free [`query_cypher_flight`] /
/// [`query_cypher_flightsql`] functions open a fresh connection per call
/// (mirroring the Python recipe), which undercounts server throughput.
pub struct LadybugClient {
    client: FlightServiceClient<Channel>,
}

impl LadybugClient {
    pub async fn connect(uri: &str) -> anyhow::Result<Self> {
        Ok(Self {
            client: connect(uri).await?,
        })
    }

    /// Plain Flight: `GetFlightInfo(new_cmd(cypher))` -> `DoGet`.
    pub async fn query_flight(
        &mut self,
        cypher: &str,
    ) -> anyhow::Result<(SchemaRef, Vec<RecordBatch>)> {
        let info = self
            .client
            .get_flight_info(FlightDescriptor::new_cmd(cypher.as_bytes().to_vec()))
            .await?
            .into_inner();
        let schema: SchemaRef = Arc::new(Schema::try_from(info.clone())?);
        let batches = read_endpoint(&mut self.client, &info).await?;
        Ok((schema, batches))
    }

    /// ADBC-equivalent prepared-statement flow over a reused connection.
    pub async fn query_flightsql(
        &mut self,
        cypher: &str,
    ) -> anyhow::Result<(SchemaRef, Vec<RecordBatch>)> {
        // 1. CreatePreparedStatement action.
        let mut action_stream = self
            .client
            .do_action(Action {
                r#type: "CreatePreparedStatement".to_string(),
                body: action_create_prepared_statement_request(cypher).into(),
            })
            .await?
            .into_inner();
        let mut bodies = Vec::new();
        while let Some(result) = action_stream.message().await? {
            bodies.push(result.body.to_vec());
        }
        let Some(first) = bodies.into_iter().next() else {
            anyhow::bail!("CreatePreparedStatement returned no result");
        };
        let (handle, _dataset_schema) = parse_create_prepared_statement_result(&first);
        if handle.is_empty() {
            anyhow::bail!("CreatePreparedStatement returned empty handle");
        }

        // 2. GetFlightInfo with CommandPreparedStatementQuery{handle}.
        let info = self
            .client
            .get_flight_info(FlightDescriptor::new_cmd(
                any_command_prepared_statement_query(&handle),
            ))
            .await?
            .into_inner();
        let schema: SchemaRef = Arc::new(Schema::try_from(info.clone())?);

        // 3. DoGet with TicketStatementQuery{handle}.
        let batches = read_endpoint(&mut self.client, &info).await?;

        // 4. ClosePreparedStatement (best-effort).
        let mut close_stream = self
            .client
            .do_action(Action {
                r#type: "ClosePreparedStatement".to_string(),
                body: action_close_prepared_statement_request(&handle).into(),
            })
            .await?
            .into_inner();
        while close_stream.message().await?.is_some() {}

        Ok((schema, batches))
    }
}

async fn read_endpoint(
    client: &mut FlightServiceClient<Channel>,
    info: &FlightInfo,
) -> anyhow::Result<Vec<RecordBatch>> {
    let mut batches = Vec::new();
    for endpoint in &info.endpoint {
        let ticket = endpoint
            .ticket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("flight endpoint missing ticket"))?;
        let resp = client.do_get(ticket).await?;
        let mut stream = FlightRecordBatchStream::new_from_flight_data(
            resp.into_inner()
                .map_err(arrow_flight::error::FlightError::from),
        );
        while let Some(batch) = stream.try_next().await? {
            batches.push(batch);
        }
    }
    Ok(batches)
}

/// Run Cypher over plain Arrow Flight; return native Arrow batches.
pub async fn query_cypher_flight(
    uri: &str,
    cypher: &str,
) -> anyhow::Result<(SchemaRef, Vec<RecordBatch>)> {
    let mut client = connect(uri).await?;
    let info = client
        .get_flight_info(FlightDescriptor::new_cmd(cypher.as_bytes().to_vec()))
        .await?
        .into_inner();
    let schema: SchemaRef = Arc::new(Schema::try_from(info.clone())?);
    let batches = read_endpoint(&mut client, &info).await?;
    Ok((schema, batches))
}

/// Run Cypher over the Flight SQL prepared-statement flow (ADBC equivalent).
///
/// Wire-for-wire what `adbc_driver_flightsql.dbapi` does: the `query` field of
/// `CommandStatementQuery` / `ActionCreatePreparedStatementRequest` is just a
/// string, so Cypher passes straight through.
pub async fn query_cypher_flightsql(
    uri: &str,
    cypher: &str,
) -> anyhow::Result<(SchemaRef, Vec<RecordBatch>)> {
    let mut client = connect(uri).await?;

    // 1. CreatePreparedStatement action.
    let mut action_stream = client
        .do_action(Action {
            r#type: "CreatePreparedStatement".to_string(),
            body: action_create_prepared_statement_request(cypher).into(),
        })
        .await?
        .into_inner();
    let mut bodies = Vec::new();
    while let Some(result) = action_stream.message().await? {
        bodies.push(result.body.to_vec());
    }
    let Some(first) = bodies.into_iter().next() else {
        anyhow::bail!("CreatePreparedStatement returned no result");
    };
    let (handle, _dataset_schema) = parse_create_prepared_statement_result(&first);
    if handle.is_empty() {
        anyhow::bail!("CreatePreparedStatement returned empty handle");
    }

    // 2. GetFlightInfo with CommandPreparedStatementQuery{handle}.
    let info = client
        .get_flight_info(FlightDescriptor::new_cmd(
            any_command_prepared_statement_query(&handle),
        ))
        .await?
        .into_inner();
    let schema: SchemaRef = Arc::new(Schema::try_from(info.clone())?);

    // 3. DoGet with TicketStatementQuery{handle} (ticket comes from the endpoint).
    let batches = read_endpoint(&mut client, &info).await?;

    // 4. ClosePreparedStatement (best-effort).
    let mut close_stream = client
        .do_action(Action {
            r#type: "ClosePreparedStatement".to_string(),
            body: action_close_prepared_statement_request(&handle).into(),
        })
        .await?
        .into_inner();
    while close_stream.message().await?.is_some() {}

    Ok((schema, batches))
}
