//! Concurrent `sql_with_bindings` against one shared client.
//!
//! Arrow Flight SQL's `prepare()` / `PreparedStatement::execute()` drop the
//! DoAction(CreatePreparedStatement) and DoPut(bind) response streams before
//! gRPC trailers arrive. That makes h2 RST_STREAM CANCEL, then count late
//! trailers as a local reset. After 1024 local resets on one connection, h2
//! sends GOAWAY ENHANCE_YOUR_CALM (`too_many_internal_resets`) and every
//! in-flight call on that connection fails.
//!
//! This file hosts an in-process Flight SQL server and issues enough concurrent
//! bound lookups on one `Client` to exceed that lifetime counter. Draining both
//! response streams to end keeps the count at zero; the pre-fix client fails
//! here and the drained client must not.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use arrow::array::{ArrayRef, Int32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::sql::{
    ActionCreatePreparedStatementResult, CommandPreparedStatementQuery,
    DoPutPreparedStatementResult, ProstMessageExt,
};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use bytes::Bytes;
use futures::future::join_all;
use futures::{Stream, StreamExt, TryStreamExt, stream};
use prost::Message;
use spiceai::{ClientBuilder, QueryParameters};
use tokio::sync::Semaphore;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

const WORKERS: usize = 64;
const TOTAL_LOOKUPS: usize = 3072;
const ORIGINAL_HANDLE: &[u8] = b"statement-1";
const BOUND_HANDLE: &[u8] = b"statement-1-bound";

type Boxed<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Yields `item`, then waits before ending so trailers are not batched with
/// the first message. An early-drop client RSTs in that window; a draining
/// client waits for the end and receives trailers on an open stream.
fn message_then_trailer_gap<T>(item: T) -> Boxed<T>
where
    T: Send + 'static,
{
    Box::pin(stream::unfold(Some(item), |state| async move {
        match state {
            Some(item) => Some((Ok(item), None)),
            None => {
                tokio::time::sleep(Duration::from_millis(2)).await;
                None
            }
        }
    }))
}

fn one_row() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "value",
        DataType::Int32,
        false,
    )]));
    let column: ArrayRef = Arc::new(Int32Array::from(vec![1]));
    RecordBatch::try_new(schema, vec![column]).expect("batch")
}

#[derive(Default)]
struct Counters {
    create: AtomicUsize,
    bind: AtomicUsize,
    info: AtomicUsize,
    get: AtomicUsize,
}

struct PreparedStatementServer {
    counters: Arc<Counters>,
}

#[tonic::async_trait]
impl FlightService for PreparedStatementServer {
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
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        self.counters.info.fetch_add(1, Ordering::Relaxed);
        let descriptor = request.into_inner();
        if let Ok(cmd) = CommandPreparedStatementQuery::try_decode(&descriptor.cmd)
            && cmd.prepared_statement_handle.as_ref() != BOUND_HANDLE
        {
            return Err(Status::invalid_argument(format!(
                "GetFlightInfo must use the handle DoPut returned, got {:?}",
                cmd.prepared_statement_handle
            )));
        }
        let info = FlightInfo::new()
            .with_endpoint(FlightEndpoint::new().with_ticket(Ticket::new("results")));
        Ok(Response::new(info))
    }

    async fn do_get(
        &self,
        _request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        self.counters.get.fetch_add(1, Ordering::Relaxed);
        let data = FlightDataEncoderBuilder::new()
            .build(stream::iter([Ok(one_row())]))
            .map_err(|error| Status::internal(error.to_string()));
        Ok(Response::new(Box::pin(data)))
    }

    async fn do_put(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        self.counters.bind.fetch_add(1, Ordering::Relaxed);
        let mut incoming = request.into_inner();
        while incoming.message().await?.is_some() {}
        let result = DoPutPreparedStatementResult {
            prepared_statement_handle: Some(Bytes::from_static(BOUND_HANDLE)),
        };
        Ok(Response::new(message_then_trailer_gap(PutResult {
            app_metadata: result.encode_to_vec().into(),
        })))
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        match request.into_inner().r#type.as_str() {
            "CreatePreparedStatement" => {
                self.counters.create.fetch_add(1, Ordering::Relaxed);
                let result = ActionCreatePreparedStatementResult {
                    prepared_statement_handle: Bytes::from_static(ORIGINAL_HANDLE),
                    dataset_schema: Bytes::new(),
                    parameter_schema: Bytes::new(),
                };
                let body: Bytes = result.as_any().encode_to_vec().into();
                Ok(Response::new(message_then_trailer_gap(
                    arrow_flight::Result { body },
                )))
            }
            "ClosePreparedStatement" => Ok(Response::new(Box::pin(stream::empty()))),
            other => Err(Status::unimplemented(format!("action {other}"))),
        }
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

trait TryDecodeCommand {
    fn try_decode(cmd: &[u8]) -> Result<Self, Status>
    where
        Self: Sized;
}

impl TryDecodeCommand for CommandPreparedStatementQuery {
    fn try_decode(cmd: &[u8]) -> Result<Self, Status> {
        use arrow_flight::sql::Any;
        let any = Any::decode(cmd).map_err(|error| Status::invalid_argument(error.to_string()))?;
        any.unpack()
            .map_err(|error| Status::invalid_argument(error.to_string()))?
            .ok_or_else(|| {
                Status::invalid_argument("descriptor is not CommandPreparedStatementQuery")
            })
    }
}

async fn start_server() -> (String, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = PreparedStatementServer {
        counters: Arc::clone(&counters),
    };
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(FlightServiceServer::new(server))
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    (format!("http://{addr}"), counters)
}

/// One shared client, 64 workers, more bound lookups than h2's 1024 local-reset
/// lifetime limit. Any early-dropped DoAction/DoPut stream fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sql_with_bindings_does_not_goaway() {
    let (url, counters) = start_server().await;
    let client = ClientBuilder::new()
        .flight_url(&url)
        .max_retries(0)
        .build()
        .await
        .expect("client");

    let limiter = Arc::new(Semaphore::new(WORKERS));
    let tasks = (0..TOTAL_LOOKUPS).map(|i| {
        let client = client.clone();
        let limiter = Arc::clone(&limiter);
        tokio::spawn(async move {
            let _permit = limiter.acquire().await.expect("permit");
            let mut stream = client
                .sql_with_bindings("SELECT $1", QueryParameters::new().push(i as i32))
                .await?;
            let mut rows = 0;
            while let Some(batch) = stream.next().await {
                rows += batch?.num_rows();
            }
            Ok::<usize, spiceai::SpiceClientError>(rows)
        })
    });

    let mut failures = Vec::new();
    let mut rows = 0;
    for (index, result) in join_all(tasks).await.into_iter().enumerate() {
        match result {
            Ok(Ok(n)) => rows += n,
            Ok(Err(error)) => failures.push(format!("{index}: {error}")),
            Err(error) => failures.push(format!("{index}: join {error}")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {TOTAL_LOOKUPS} bound lookups failed; first {}: {:?}",
        failures.len(),
        failures.len().min(8),
        &failures[..failures.len().min(8)]
    );
    assert_eq!(rows, TOTAL_LOOKUPS);
    assert_eq!(counters.create.load(Ordering::Relaxed), TOTAL_LOOKUPS);
    assert_eq!(counters.bind.load(Ordering::Relaxed), TOTAL_LOOKUPS);
    assert_eq!(counters.info.load(Ordering::Relaxed), TOTAL_LOOKUPS);
    assert_eq!(counters.get.load(Ordering::Relaxed), TOTAL_LOOKUPS);
}
