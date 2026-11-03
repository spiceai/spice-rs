//! The client's Flight receive limit, exercised against a loopback server that sends a
//! result batch as one message larger than gRPC's 4 MiB default — as a runtime that does
//! not split large batches does.
//!
//! These tests need no runtime, so they run on every pull request.

use std::pin::Pin;
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use futures::{Stream, StreamExt, TryStreamExt, stream};
use spiceai::{ClientBuilder, DEFAULT_MAX_DECODING_MESSAGE_SIZE};
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

/// gRPC's default receive limit, which the client applied before it was configurable.
const GRPC_DEFAULT_LIMIT: usize = 4 * 1024 * 1024;

const ROWS: usize = 25_000;

/// About 6 MiB of rows: over gRPC's 4 MiB default, well under 100 MiB.
fn large_batch() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("s", DataType::Utf8, false),
    ]));
    let pad = "x".repeat(240);
    let ids: ArrayRef = Arc::new(Int64Array::from_iter_values(
        (0..ROWS).map(|i| i64::try_from(i).expect("row id")),
    ));
    let strings: ArrayRef = Arc::new(StringArray::from_iter_values(
        (0..ROWS).map(|i| format!("{i:09}{pad}")),
    ));
    RecordBatch::try_new(schema, vec![ids, strings]).expect("batch")
}

type Boxed<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Answers every query with [`large_batch`] as a single Flight message.
struct OneLargeMessageServer;

#[tonic::async_trait]
impl FlightService for OneLargeMessageServer {
    type HandshakeStream = Boxed<HandshakeResponse>;
    type ListFlightsStream = Boxed<FlightInfo>;
    type DoGetStream = Boxed<FlightData>;
    type DoPutStream = Boxed<PutResult>;
    type DoActionStream = Boxed<arrow_flight::Result>;
    type ListActionsStream = Boxed<ActionType>;
    type DoExchangeStream = Boxed<FlightData>;

    async fn handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        Err(Status::unimplemented("handshake"))
    }

    async fn get_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let info = FlightInfo::new()
            .with_endpoint(FlightEndpoint::new().with_ticket(Ticket::new("results")));
        Ok(Response::new(info))
    }

    async fn do_get(
        &self,
        _request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        let data = FlightDataEncoderBuilder::new()
            .with_max_flight_data_size(usize::MAX)
            .build(stream::iter([Ok(large_batch())]))
            .map_err(|error| Status::internal(error.to_string()));
        Ok(Response::new(Box::pin(data)))
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        Err(Status::unimplemented("do_put"))
    }

    async fn do_action(
        &self,
        _request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        Err(Status::unimplemented("do_action"))
    }

    async fn list_flights(
        &self,
        _request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        Err(Status::unimplemented("list_flights"))
    }

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented("poll_flight_info"))
    }

    async fn get_schema(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        Err(Status::unimplemented("get_schema"))
    }

    async fn list_actions(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        Err(Status::unimplemented("list_actions"))
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("do_exchange"))
    }
}

/// Starts the server on a loopback port and returns its Flight URL.
async fn start_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(FlightServiceServer::new(OneLargeMessageServer))
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    format!("http://{addr}")
}

/// Rows the query returned, or the first error it raised.
async fn query_rows(builder: ClientBuilder) -> Result<usize, String> {
    let client = builder.build().await.map_err(|e| e.to_string())?;
    let mut stream = client
        .sql("SELECT * FROM large")
        .await
        .map_err(|e| e.to_string())?;
    let mut rows = 0;
    while let Some(batch) = stream.next().await {
        rows += batch.map_err(|e| e.to_string())?.num_rows();
    }
    Ok(rows)
}

#[test]
fn the_default_limit_is_the_runtimes_100_mib() {
    assert_eq!(DEFAULT_MAX_DECODING_MESSAGE_SIZE, 100 * 1024 * 1024);
}

/// A stock client reads a result message over gRPC's 4 MiB default.
#[tokio::test]
async fn default_client_reads_a_message_over_4_mib() {
    let url = start_server().await;
    let rows = query_rows(ClientBuilder::new().flight_url(&url))
        .await
        .expect("default client reads the large message");
    assert_eq!(rows, ROWS);
}

/// The limit is the one the builder sets: a client held to gRPC's 4 MiB default fails
/// on the same message, naming the limit.
#[tokio::test]
async fn configured_limit_is_applied() {
    let url = start_server().await;
    let error = query_rows(
        ClientBuilder::new()
            .flight_url(&url)
            .max_decoding_message_size(GRPC_DEFAULT_LIMIT),
    )
    .await
    .expect_err("a 4 MiB limit rejects the large message");
    assert!(
        error.contains("decoded message length too large") && error.contains("4194304"),
        "unexpected error: {error}"
    );

    let rows = query_rows(
        ClientBuilder::new()
            .flight_url(&url)
            .max_decoding_message_size(16 * 1024 * 1024),
    )
    .await
    .expect("a 16 MiB limit reads it");
    assert_eq!(rows, ROWS);
}
