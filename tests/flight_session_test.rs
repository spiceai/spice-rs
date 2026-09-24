//! The client's Flight session, exercised against a loopback server that enforces the
//! runtime's rule: every RPC after the handshake must carry the bearer token the
//! handshake issued, and the handshake must carry the api key.
//!
//! These tests need no runtime, so they run on every pull request.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arrow::array::{ArrayRef, Int32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::flight_service_server::{FlightService, FlightServiceServer};
use arrow_flight::sql::{ActionCreatePreparedStatementResult, ProstMessageExt};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use bytes::Bytes;
use futures::future::join_all;
use futures::{Stream, StreamExt, TryStreamExt, stream};
use prost::Message;
use spiceai::{ClientBuilder, QueryParameters};
use tonic::metadata::MetadataMap;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

const API_KEY: &str = "a-valid-api-key";

/// One request the server admitted or refused, other than a handshake.
#[derive(Clone, Debug)]
struct Seen {
    rpc: &'static str,
    authorization: Option<String>,
    user_agent: Option<String>,
}

#[derive(Default)]
struct Ledger {
    handshake_attempts: AtomicUsize,
    handshakes: AtomicUsize,
    /// Tokens the server still honours.
    sessions: Mutex<HashSet<String>>,
    /// Refuse every bearer token, as a runtime whose session store forgot them would.
    refuse_sessions: AtomicBool,
    requests: Mutex<Vec<Seen>>,
}

impl Ledger {
    fn handshakes(&self) -> usize {
        self.handshakes.load(Ordering::SeqCst)
    }

    fn handshake_attempts(&self) -> usize {
        self.handshake_attempts.load(Ordering::SeqCst)
    }

    /// Expires every session the server issued, as an hour of inactivity or a runtime
    /// restart would.
    fn expire_sessions(&self) {
        self.sessions.lock().expect("sessions").clear();
    }

    fn requests(&self) -> Vec<Seen> {
        self.requests.lock().expect("requests").clone()
    }
}

struct SessionServer {
    ledger: Arc<Ledger>,
}

type Boxed<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

impl SessionServer {
    /// Mirrors the runtime's `BasicAuthMiddleware`, which answers every request that
    /// lacks a bearer token it recognises with `UNAUTHENTICATED`.
    fn admit(&self, rpc: &'static str, metadata: &MetadataMap) -> Result<(), Status> {
        let header = |name: &str| {
            metadata
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let authorization = header("authorization");
        self.ledger.requests.lock().expect("requests").push(Seen {
            rpc,
            authorization: authorization.clone(),
            user_agent: header("user-agent"),
        });

        let Some(authorization) = authorization else {
            return Err(Status::unauthenticated("Missing authorization header"));
        };
        let Some(token) = authorization.strip_prefix("Bearer ") else {
            return Err(Status::unauthenticated("Invalid authorization header"));
        };
        let honoured = !self.ledger.refuse_sessions.load(Ordering::SeqCst)
            && self
                .ledger
                .sessions
                .lock()
                .expect("sessions")
                .contains(token);
        if honoured {
            Ok(())
        } else {
            Err(Status::unauthenticated("Invalid credentials"))
        }
    }
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

#[tonic::async_trait]
impl FlightService for SessionServer {
    type HandshakeStream = Boxed<HandshakeResponse>;
    type ListFlightsStream = Boxed<FlightInfo>;
    type DoGetStream = Boxed<FlightData>;
    type DoPutStream = Boxed<PutResult>;
    type DoActionStream = Boxed<arrow_flight::Result>;
    type ListActionsStream = Boxed<ActionType>;
    type DoExchangeStream = Boxed<FlightData>;

    async fn handshake(
        &self,
        request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        self.ledger
            .handshake_attempts
            .fetch_add(1, Ordering::SeqCst);
        let expected = format!("Basic {}", BASE64_STANDARD.encode(format!(":{API_KEY}")));
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        if presented != Some(expected.as_str()) {
            return Err(Status::unauthenticated("Invalid credentials"));
        }

        let issued = self.ledger.handshakes.fetch_add(1, Ordering::SeqCst) + 1;
        let token = format!("session-{issued}");
        self.ledger
            .sessions
            .lock()
            .expect("sessions")
            .insert(token.clone());

        let payload: Bytes = token.clone().into();
        let mut response: Response<Self::HandshakeStream> =
            Response::new(Box::pin(stream::iter(vec![Ok(HandshakeResponse {
                protocol_version: 0,
                payload,
            })])));
        response.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("header value"),
        );
        Ok(response)
    }

    async fn get_flight_info(
        &self,
        request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        self.admit("get_flight_info", request.metadata())?;
        let info = FlightInfo::new()
            .with_endpoint(FlightEndpoint::new().with_ticket(Ticket::new("results")));
        Ok(Response::new(info))
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        self.admit("do_get", request.metadata())?;
        let data = FlightDataEncoderBuilder::new()
            .build(stream::iter([Ok(one_row())]))
            .map_err(|error| Status::internal(error.to_string()));
        Ok(Response::new(Box::pin(data)))
    }

    async fn do_put(
        &self,
        request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        self.admit("do_put", request.metadata())?;
        let mut incoming = request.into_inner();
        while incoming.message().await?.is_some() {}
        Ok(Response::new(Box::pin(stream::empty())))
    }

    async fn do_action(
        &self,
        request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        self.admit("do_action", request.metadata())?;
        match request.into_inner().r#type.as_str() {
            "CreatePreparedStatement" => {
                let result = ActionCreatePreparedStatementResult {
                    prepared_statement_handle: Bytes::from_static(b"statement-1"),
                    dataset_schema: Bytes::new(),
                    parameter_schema: Bytes::new(),
                };
                let body: Bytes = result.as_any().encode_to_vec().into();
                Ok(Response::new(Box::pin(stream::iter([Ok(
                    arrow_flight::Result { body },
                )]))))
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

/// Starts the server on a loopback port and returns its Flight URL and ledger.
async fn start_server() -> (String, Arc<Ledger>) {
    let ledger = Arc::new(Ledger::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = SessionServer {
        ledger: Arc::clone(&ledger),
    };
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(FlightServiceServer::new(server))
            .serve_with_incoming(TcpIncoming::from(listener)),
    );
    (format!("http://{addr}"), ledger)
}

async fn client(url: &str, api_key: &str) -> spiceai::Client {
    ClientBuilder::new()
        .flight_url(url)
        .api_key(api_key)
        .build()
        .await
        .expect("client")
}

async fn count_rows<S, E>(mut stream: S) -> usize
where
    S: Stream<Item = Result<RecordBatch, E>> + Unpin,
    E: std::fmt::Debug,
{
    let mut rows = 0;
    while let Some(batch) = stream.next().await {
        rows += batch.expect("batch").num_rows();
    }
    rows
}

/// Runs `n` queries that all start before any of them finishes.
async fn concurrent_queries(client: &spiceai::Client, n: usize) {
    for result in join_all((0..n).map(|_| client.sql("SELECT 1"))).await {
        assert_eq!(count_rows(result.expect("query")).await, 1);
    }
}

fn seen(ledger: &Ledger, rpc: &str) -> Vec<Seen> {
    ledger
        .requests()
        .into_iter()
        .filter(|request| request.rpc == rpc)
        .collect()
}

/// One handshake serves every query a client makes: the runtime keeps the session,
/// so a second handshake per query is a round trip that buys nothing.
#[tokio::test]
async fn sql_handshakes_once_and_reuses_the_session() {
    let (url, ledger) = start_server().await;
    let client = client(&url, API_KEY).await;

    for _ in 0..5 {
        let stream = client.sql("SELECT 1").await.expect("query");
        assert_eq!(count_rows(stream).await, 1);
    }

    assert_eq!(
        ledger.handshakes(),
        1,
        "five queries must share one handshake"
    );
    let gets = seen(&ledger, "do_get");
    assert_eq!(gets.len(), 5);
    for get in gets {
        assert_eq!(get.authorization.as_deref(), Some("Bearer session-1"));
    }
}

/// A parameterized query has to reach the runtime under the same session and user
/// agent as a plain one; a runtime with an api key refuses it otherwise.
#[tokio::test]
async fn parameterized_queries_are_sent_under_the_session() {
    let (url, ledger) = start_server().await;
    let client = client(&url, API_KEY).await;

    let stream = client
        .sql_with_bindings("SELECT $1", QueryParameters::new().push(1_i32))
        .await
        .expect("a parameterized query authenticates like a plain one");
    assert_eq!(count_rows(stream).await, 1);

    assert_eq!(ledger.handshakes(), 1);
    let requests = ledger.requests();
    let rpcs: Vec<&str> = requests.iter().map(|request| request.rpc).collect();
    assert_eq!(rpcs, ["do_action", "do_put", "get_flight_info", "do_get"]);
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer session-1"),
            "{} must carry the session",
            request.rpc
        );
        let user_agent = request.user_agent.as_deref().unwrap_or_default();
        assert!(
            user_agent.contains("spice-rs/"),
            "{} must carry the SDK user agent, got {user_agent:?}",
            request.rpc
        );
    }

    let stream = client
        .sql_with_bindings("SELECT $1", QueryParameters::new().push(2_i32))
        .await
        .expect("second parameterized query");
    assert_eq!(count_rows(stream).await, 1);
    assert_eq!(
        ledger.handshakes(),
        1,
        "parameterized queries reuse the session too"
    );
}

/// A session the runtime has forgotten is renewed once, transparently, and the query
/// that found it expired still succeeds.
#[tokio::test]
async fn an_expired_session_is_renewed_once() {
    let (url, ledger) = start_server().await;
    let client = client(&url, API_KEY).await;

    let stream = client.sql("SELECT 1").await.expect("first query");
    assert_eq!(count_rows(stream).await, 1);

    ledger.expire_sessions();

    let stream = client
        .sql("SELECT 1")
        .await
        .expect("an expired session is renewed, not surfaced");
    assert_eq!(count_rows(stream).await, 1);
    assert_eq!(ledger.handshakes(), 2);

    let infos: Vec<Option<String>> = seen(&ledger, "get_flight_info")
        .into_iter()
        .map(|request| request.authorization)
        .collect();
    assert_eq!(
        infos,
        [
            Some("Bearer session-1".to_string()),
            Some("Bearer session-1".to_string()),
            Some("Bearer session-2".to_string()),
        ],
        "the stale token is presented once, refused, and replaced"
    );

    let stream = client
        .sql_with_bindings("SELECT $1", QueryParameters::new().push(1_i32))
        .await
        .expect("the renewed session serves parameterized queries");
    assert_eq!(count_rows(stream).await, 1);
    assert_eq!(ledger.handshakes(), 2);
}

/// A parameterized query renews an expired session the same way.
#[tokio::test]
async fn a_parameterized_query_renews_an_expired_session() {
    let (url, ledger) = start_server().await;
    let client = client(&url, API_KEY).await;

    let stream = client.sql("SELECT 1").await.expect("first query");
    assert_eq!(count_rows(stream).await, 1);
    ledger.expire_sessions();

    let stream = client
        .sql_with_bindings("SELECT $1", QueryParameters::new().push(1_i32))
        .await
        .expect("renewed");
    assert_eq!(count_rows(stream).await, 1);
    assert_eq!(ledger.handshakes(), 2);
}

/// A credential the runtime refuses is refused once. Renewing it would handshake in
/// a loop against the server.
#[tokio::test]
async fn a_rejected_api_key_is_not_retried() {
    let (url, ledger) = start_server().await;
    let client = client(&url, "not-the-key").await;

    let error = client
        .sql("SELECT 1")
        .await
        .err()
        .expect("a refused credential fails the query");
    assert!(
        error.to_string().contains("Invalid credentials"),
        "the runtime's explanation must reach the caller, got {error}"
    );
    assert_eq!(ledger.handshake_attempts(), 1);
    assert_eq!(ledger.handshakes(), 0);
    assert!(
        seen(&ledger, "get_flight_info").is_empty(),
        "no query is sent without a session"
    );
}

/// A token the runtime issued and immediately refuses cannot have expired, so it is
/// not renewed: one handshake, one refusal, and the error reaches the caller.
#[tokio::test]
async fn a_fresh_session_the_runtime_refuses_is_not_renewed() {
    let (url, ledger) = start_server().await;
    ledger.refuse_sessions.store(true, Ordering::SeqCst);
    let client = client(&url, API_KEY).await;

    let error = client
        .sql("SELECT 1")
        .await
        .err()
        .expect("a refused session fails the query");
    assert!(error.to_string().contains("Invalid credentials"), "{error}");
    assert_eq!(ledger.handshakes(), 1, "a fresh token is not renewed");
    assert_eq!(seen(&ledger, "get_flight_info").len(), 1);

    let error = client
        .sql_with_bindings("SELECT $1", QueryParameters::new().push(1_i32))
        .await
        .err()
        .expect("a refused session fails the parameterized query");
    assert!(error.to_string().contains("Invalid credentials"), "{error}");
    assert_eq!(
        ledger.handshakes(),
        2,
        "the reused token is renewed once, and the renewed one is not"
    );
}

/// Queries that start together must share the one handshake, not each pay for their
/// own: the session is per client, so a client's first burst is where a handshake per
/// query costs the most.
#[tokio::test]
async fn concurrent_first_queries_share_one_handshake() {
    let (url, ledger) = start_server().await;
    let client = client(&url, API_KEY).await;

    concurrent_queries(&client, 5).await;

    assert_eq!(
        ledger.handshakes(),
        1,
        "five queries that start together must share one handshake"
    );
    let gets = seen(&ledger, "do_get");
    assert_eq!(gets.len(), 5);
    for get in gets {
        assert_eq!(get.authorization.as_deref(), Some("Bearer session-1"));
    }
}

/// An expired session is renewed once for the whole burst that finds it expired -- a
/// handshake per rejected query would answer an hour of inactivity with a storm.
#[tokio::test]
async fn concurrent_queries_renew_an_expired_session_once() {
    let (url, ledger) = start_server().await;
    let client = client(&url, API_KEY).await;

    let stream = client.sql("SELECT 1").await.expect("query");
    assert_eq!(count_rows(stream).await, 1);
    assert_eq!(ledger.handshakes(), 1);

    ledger.expire_sessions();

    concurrent_queries(&client, 5).await;

    assert_eq!(
        ledger.handshakes(),
        2,
        "the burst that found the session expired must renew it once between them"
    );
}
