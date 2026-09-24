use crate::client::Error as SpiceClientError;
use crate::config::GenericError;
use crate::config::get_user_agent;
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use arrow_flight::FlightDescriptor;
use arrow_flight::HandshakeRequest;
use arrow_flight::decode::FlightRecordBatchStream;
use arrow_flight::error::FlightError;
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::sql::client::FlightSqlServiceClient;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use bytes::Bytes;
use futures::Future;
use futures::Stream;
use futures::TryStreamExt;
use futures::stream;
use futures::task::Context;
use futures::task::Poll;
use snafu::Snafu;
use std::collections::HashMap;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use tonic::IntoRequest;
use tonic::metadata::AsciiMetadataKey;
use tonic::transport::Channel;

/// What the runtime answered on the client's most recent handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Session {
    /// No handshake has completed yet -- or one is in flight, which a reader cannot
    /// tell apart from here: `handshake_gate` is what distinguishes them, so a burst
    /// must take it rather than act on `Pending` alone.
    Pending,
    /// A handshake completed. The runtime issues a bearer token when it authenticated
    /// the credential, and none when it runs without authentication.
    Established(Option<Arc<str>>),
}

/// The credential a request is sent under.
struct Credential {
    /// The bearer token, or none when the runtime issued no token or no api key is
    /// configured.
    token: Option<Arc<str>>,
    /// Whether the token was reused from an earlier handshake. Only a reused token can
    /// have expired, so only a reused token is worth renewing when the runtime rejects it.
    reused: bool,
}

#[derive(Clone)]
pub struct SqlFlightClient {
    headers: Arc<HashMap<String, String>>,
    client: FlightServiceClient<Channel>,
    api_key: Option<Arc<str>>,
    max_retries: u32,
    /// The session the runtime issued for `api_key`, shared by every clone of this
    /// client so a retry or a concurrent query reuses it rather than handshaking again.
    session: Arc<Mutex<Session>>,
    /// Serializes the handshake itself, so a burst of queries that all find no session
    /// shares one round trip rather than each opening its own. Take it before `session`
    /// and never the other way round: it is held across the handshake, which is why it is
    /// async, whereas `session` is only ever held to read or replace it.
    handshake_gate: Arc<tokio::sync::Mutex<()>>,
}

impl SqlFlightClient {
    pub fn new(
        chan: Channel,
        api_key: Option<String>,
        user_agent: Option<String>,
        cache_control: Option<String>,
        max_retries: u32,
    ) -> Self {
        // Prepend the user agent with the provided user agent if it exists
        let user_agent = match user_agent {
            Some(ua) => format!("{ua} {}", get_user_agent()),
            None => get_user_agent(),
        };

        let mut headers = HashMap::new();
        headers.insert("User-Agent".to_string(), user_agent);

        if let Some(cache_control) = cache_control {
            headers.insert("Cache-Control".to_string(), cache_control);
        }

        SqlFlightClient {
            api_key: api_key.map(|s| Arc::from(s.into_boxed_str())),
            headers: Arc::new(headers),
            client: FlightServiceClient::new(chan),
            max_retries,
            session: Arc::new(Mutex::new(Session::Pending)),
            handshake_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn handshake(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, GenericError> {
        let cmd = HandshakeRequest {
            protocol_version: 0,
            payload: Bytes::default(),
        };
        let mut req = tonic::Request::new(stream::iter(vec![cmd]));
        let val = BASE64_STANDARD.encode(format!("{username}:{password}"));
        let val = format!("Basic {val}")
            .parse()
            .map_err(|_| ArrowError::ParseError("Cannot parse header".to_string()))?;
        req.metadata_mut().insert("authorization", val);
        let req = self.set_request_headers(req, None)?;
        let resp = self
            .client
            .clone()
            .handshake(req)
            .await
            .map_err(|source| HandshakeError { source })?;

        let mut token: Option<String> = None;
        if let Some(auth) = resp.metadata().get("authorization") {
            let auth = auth
                .to_str()
                .map_err(|_| ArrowError::ParseError("Can't read auth header".to_string()))?;
            let bearer = "Bearer ";
            if !auth.starts_with(bearer) {
                Err(ArrowError::ParseError("Invalid auth header!".to_string()))?;
            }
            let auth = auth[bearer.len()..].to_string();
            token = Some(auth);
        }
        Ok(token)
    }

    fn cached_session(&self) -> Session {
        self.session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Call this under `handshake_gate`. Today `authenticate` is the only caller and
    /// holds it; a second writer that does not would reinstate a handshake per query in
    /// a burst, with nothing to fail on it.
    fn store_session(&self, token: Option<Arc<str>>) {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Session::Established(token);
    }

    /// Forgets `stale`, unless another request has already replaced it: a renewal that
    /// raced this one must not be thrown away for a third handshake.
    fn forget_session(&self, stale: Option<&Arc<str>>) {
        let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*session, Session::Established(current) if current.as_ref() == stale) {
            *session = Session::Pending;
        }
    }

    /// Resolves the credential to send a request under, handshaking only when no
    /// session is established yet.
    ///
    /// The runtime answers the handshake with a session token and keeps that session
    /// for an hour of inactivity, so one handshake serves every query a client makes
    /// rather than each one paying a round trip of its own.
    ///
    /// Queries that start together share that handshake rather than each opening one:
    /// reading `session` releases it before the round trip, so without `handshake_gate`
    /// every query in a client's first burst would see `Pending` and handshake, and every
    /// query in a burst that finds the session expired would renew it separately. A
    /// handshake that *fails* is serialized by the same gate rather than retried in
    /// parallel, which is the cost of the guarantee.
    async fn authenticate(&self) -> std::result::Result<Credential, GenericError> {
        let (username, password) = match &self.api_key {
            Some(api_key) => ("", api_key.as_ref()),
            None => {
                return Ok(Credential {
                    token: None,
                    reused: false,
                });
            }
        };

        if let Session::Established(token) = self.cached_session() {
            return Ok(Credential {
                token,
                reused: true,
            });
        }

        let _handshaking = self.handshake_gate.lock().await;

        // Not `reused`: this token was issued while the call waited on the gate, so it
        // cannot have expired, and renewing on its rejection would answer a credential
        // the runtime refuses with a second handshake.
        if let Session::Established(token) = self.cached_session() {
            return Ok(Credential {
                token,
                reused: false,
            });
        }

        let token: Option<Arc<str>> = self.handshake(username, password).await?.map(Arc::from);
        self.store_session(token.clone());

        Ok(Credential {
            token,
            reused: false,
        })
    }

    /// Runs `request` under the client's session, renewing the session once when the
    /// runtime no longer recognises a reused token.
    ///
    /// A session outlives neither an hour of inactivity nor a runtime restart, and a
    /// client that outlives either is answered `UNAUTHENTICATED` until it handshakes
    /// again. A token this call has just obtained cannot have expired, so its rejection
    /// is returned as is: renewing it would turn a credential the runtime refuses into
    /// a loop of handshakes.
    async fn with_session<F, Fut>(
        &self,
        request: F,
    ) -> std::result::Result<FlightRecordBatchStream, GenericError>
    where
        F: Fn(Option<Arc<str>>) -> Fut,
        Fut: Future<Output = std::result::Result<FlightRecordBatchStream, GenericError>>,
    {
        let credential = self.authenticate().await?;
        match request(credential.token.clone()).await {
            Err(error) if credential.reused && is_unauthenticated(&error) => {
                self.forget_session(credential.token.as_ref());
                let renewed = self.authenticate().await?;
                request(renewed.token).await
            }
            result => result,
        }
    }

    fn set_request_headers<T>(
        &self,
        mut req: tonic::Request<T>,
        token: Option<&str>,
    ) -> Result<tonic::Request<T>, ArrowError> {
        for (k, v) in self.headers.iter() {
            let k = AsciiMetadataKey::from_str(k.as_str()).map_err(|e| {
                ArrowError::ParseError(format!("Cannot convert header key \"{k}\": {e}"))
            })?;
            let v = v.parse().map_err(|e| {
                ArrowError::ParseError(format!("Cannot convert header value \"{v}\": {e}"))
            })?;
            req.metadata_mut().insert(k, v);
        }
        if let Some(token) = token {
            let val = format!("Bearer {token}").parse().map_err(|e| {
                ArrowError::ParseError(format!("Cannot convert token to header value: {e}"))
            })?;
            req.metadata_mut().insert("authorization", val);
        }
        Ok(req)
    }

    pub async fn query(
        &self,
        query: &str,
    ) -> std::result::Result<FlightRecordBatchStream, GenericError> {
        self.with_session(|token| self.execute_statement(query, token))
            .await
    }

    async fn execute_statement(
        &self,
        query: &str,
        token: Option<Arc<str>>,
    ) -> std::result::Result<FlightRecordBatchStream, GenericError> {
        let descriptor = FlightDescriptor::new_cmd(query.to_string());
        let req = self.set_request_headers(descriptor.into_request(), token.as_deref())?;

        let info = self.client.clone().get_flight_info(req).await?.into_inner();

        for ep in info.endpoint {
            if let Some(tkt) = ep.ticket {
                let req = tkt.into_request();
                let req = self.set_request_headers(req, token.as_deref())?;
                let (md, response_stream, _ext) =
                    self.client.clone().do_get(req).await?.into_parts();

                return Ok(FlightRecordBatchStream::new_from_flight_data(
                    response_stream.map_err(|e| FlightError::Tonic(Box::new(e))),
                )
                .with_headers(md));
            }
        }
        Err("No endpoints found".into())
    }

    pub async fn query_with_params(
        &self,
        query: &str,
        params: Option<RecordBatch>,
    ) -> std::result::Result<FlightRecordBatchStream, GenericError> {
        match params {
            Some(params) => {
                self.with_session(|token| {
                    self.execute_prepared_statement(query, params.clone(), token)
                })
                .await
            }
            None => self.query(query).await,
        }
    }

    async fn execute_prepared_statement(
        &self,
        query: &str,
        parameters: RecordBatch,
        token: Option<Arc<str>>,
    ) -> std::result::Result<FlightRecordBatchStream, GenericError> {
        let mut client = FlightSqlServiceClient::new_from_inner(self.client.clone());
        // The Flight SQL client sends only the headers it is given, so the session and
        // the user agent have to reach it the same way they reach a plain query.
        for (key, value) in self.headers.iter() {
            client.set_header(key, value);
        }
        if let Some(token) = token {
            client.set_token(token.to_string());
        }
        let mut prepared_stmt = client.prepare(query.to_string(), None).await?;

        prepared_stmt.set_parameters(parameters)?;

        let flight_info = prepared_stmt.execute().await?;

        let endpoint = flight_info
            .endpoint
            .first()
            .ok_or("No endpoint in flight info")?;

        let stream = client
            .do_get(
                endpoint
                    .ticket
                    .clone()
                    .ok_or("No flight ticket in response")?,
            )
            .await?;
        Ok(stream)
    }
}

/// Whether the runtime refused the credential a request was sent under.
///
/// A plain query surfaces the status itself; the Flight SQL client boxes it inside a
/// `FlightError`.
fn is_unauthenticated(error: &GenericError) -> bool {
    let status = if let Some(status) = error.downcast_ref::<tonic::Status>() {
        status
    } else if let Some(FlightError::Tonic(status)) = error.downcast_ref::<FlightError>() {
        status.as_ref()
    } else {
        return false;
    };
    status.code() == tonic::Code::Unauthenticated
}

/// Represents the current state of the `RetryableQueryStream` state machine.
/// Wraps a `FlightRecordBatchStream` and started from `Streaming` stage.
/// If a retryable error occurs during streaming, the stream resets and retries.
/// `Streaming` -> `Ready` → `Executing` → `Streaming` → `Ready`
/// If a non-retryable error occurs during streaming, the stream will be immediately terminated.
/// `Streaming` -> `Terminated`. (non-retryable error)
enum StreamState {
    /// Ready to retry a query
    Ready,
    /// Query is being executed, waiting for the server to return a stream
    Executing(Pin<Box<dyn Future<Output = Result<FlightRecordBatchStream, GenericError>> + Send>>),
    /// Initial state, actively streaming record batches from the server
    Streaming(Pin<Box<FlightRecordBatchStream>>),
    /// Terminal state - stream has ended due to non-retryable error
    Terminated,
}

/// A retryable stream for executing SQL queries with Flight.
///
/// This stream automatically handles streaming failures and immediately retries queries.
/// It yields `RecordBatch` results on success and `SpiceClientError` on failure.
///
/// ## Retry Behavior
///
/// When a connection reset occurs during streaming, the stream will:
/// 1. Yield a `SpiceClientError::ConnectionReset` error to the consumer
/// 2. If the consumer continues polling, automatically retry the entire query from the beginning
/// 3. If the consumer stops polling, the stream will not retry and enters the `Terminated` state
/// 4. Stop retrying and enters the `Terminated` state after reaching `max_retries` attempts
///
/// ## Consumer Options
///
/// **Option 1: Continue polling for automatic retry**
/// ```text
/// Poll 1: Ok(batch1)
/// Poll 2: Ok(batch2)
/// Poll 3: Err(ConnectionReset) → Consumer continues polling
/// Poll 4: Ok(batch1) → Query restarted from beginning
/// Poll 5: Ok(batch2)
/// Poll 6: Ok(batch3)
/// ...
/// ```
///
/// **Option 2: Stop on error**
/// ```text
/// Poll 1: Ok(batch1)
/// Poll 2: Ok(batch2)
/// Poll 3: Err(ConnectionReset) → Consumer stops polling
/// ```
///
/// ## Important Notes
/// - The query restarts from the beginning on retry - previously yielded batches will be re-yielded
/// - Non-retryable errors are returned immediately without retry attempts
/// - Only connection resets and specific gRPC errors trigger retries
///
pub struct RetryableQueryStream {
    client: Arc<SqlFlightClient>,
    sql: Arc<String>,
    params: Option<RecordBatch>,
    state: StreamState,
    max_retries: u32,
    retry_count: u32,
}

impl RetryableQueryStream {
    pub fn new(
        client: Arc<SqlFlightClient>,
        sql: &str,
        params: Option<RecordBatch>,
        stream: Pin<Box<FlightRecordBatchStream>>,
    ) -> Self {
        Self {
            max_retries: client.max_retries,
            client,
            sql: Arc::new(sql.to_string()),
            params,
            state: StreamState::Streaming(stream),
            retry_count: 0,
        }
    }
}

impl Stream for RetryableQueryStream {
    type Item = Result<RecordBatch, SpiceClientError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.state {
            StreamState::Ready => {
                let client = Arc::clone(&self.client);
                let sql = Arc::clone(&self.sql);
                let params = self.params.clone();

                let fut = Box::pin(async move { client.query_with_params(&sql, params).await });

                self.state = StreamState::Executing(fut);
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            StreamState::Executing(fut) => match fut.as_mut().poll(cx) {
                Poll::Ready(Ok(stream)) => {
                    self.state = StreamState::Streaming(Box::pin(stream));
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Ready(Err(error)) => {
                    if is_connection_reset_generic_error(&error)
                        && self.retry_count < self.max_retries
                    {
                        self.retry_count += 1;
                        self.state = StreamState::Ready;
                        cx.waker().wake_by_ref();
                        return Poll::Ready(Some(Err(SpiceClientError::ConnectionReset {
                            message: error.to_string(),
                        })));
                    }
                    self.state = StreamState::Terminated;
                    Poll::Ready(Some(Err(SpiceClientError::Query { source: error })))
                }
                Poll::Pending => Poll::Pending,
            },
            StreamState::Streaming(stream) => match stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(batch))) => Poll::Ready(Some(Ok(batch))),
                Poll::Ready(Some(Err(error))) => {
                    if is_connection_reset_flight_error(&error)
                        && self.retry_count < self.max_retries
                    {
                        self.retry_count += 1;
                        self.state = StreamState::Ready;
                        cx.waker().wake_by_ref();
                        return Poll::Ready(Some(Err(SpiceClientError::ConnectionReset {
                            message: error.to_string(),
                        })));
                    }
                    self.state = StreamState::Terminated;
                    Poll::Ready(Some(Err(SpiceClientError::QueryStream { source: error })))
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
            StreamState::Terminated => Poll::Ready(None),
        }
    }
}

/// A failed handshake, keeping the server's status reachable rather than rendering it.
///
/// The status is the only thing that says whether the failure is worth retrying, and a
/// rendered one is indistinguishable from a permanent failure. `mod flight` is private,
/// so this is not part of the crate's public API.
#[derive(Debug, Snafu)]
#[snafu(display("Can't handshake: {source}"))]
pub struct HandshakeError {
    pub source: tonic::Status,
}

/// Metadata key a server sets to mark its own error as safe to retry.
const RETRYABLE_METADATA_KEY: &str = "spiceai-retryable";

/// gRPC codes a transport reset is reported under.
const RESET_CODES: [tonic::Code; 3] = [
    tonic::Code::Internal,
    tonic::Code::Cancelled,
    tonic::Code::Unknown,
];

fn is_reset_code(code: tonic::Code) -> bool {
    RESET_CODES.contains(&code)
}

/// Message fragments that identify a transport reset within a reset code.
fn has_reset_marker(message: &str) -> bool {
    let message = message.to_lowercase();
    message.contains("operation was canceled")
        || message.contains("http2 error")
        || message.contains("grpc-status header missing")
        || message.contains("received message with invalid compression flag")
        || message.contains("error reading a body from connection")
        || message.contains("transport error")
}

pub fn is_tonic_reset_error(error: &tonic::Status) -> bool {
    is_reset_code(error.code()) && has_reset_marker(error.message())
}

fn is_retryable_status(status: &tonic::Status) -> bool {
    is_tonic_reset_error(status) || status.metadata().contains_key(RETRYABLE_METADATA_KEY)
}

fn is_connection_reset_flight_error(error: &FlightError) -> bool {
    match error {
        FlightError::Tonic(status) => is_retryable_status(status),
        _ => false,
    }
}

pub fn is_connection_reset_generic_error(error: &GenericError) -> bool {
    if let Some(status) = error.downcast_ref::<tonic::Status>() {
        return is_retryable_status(status);
    }
    // The Flight SQL client boxes its errors as `FlightError`, so neither the typed status
    // it carries nor a rendered one is reachable by downcasting to `tonic::Status`.
    if let Some(error) = error.downcast_ref::<FlightError>() {
        return is_connection_reset_flight_error(error);
    }
    if let Some(error) = error.downcast_ref::<HandshakeError>() {
        return is_retryable_status(&error.source);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_tonic_reset_error_internal_with_http2() {
        let status = tonic::Status::internal("http2 error occurred");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_internal_with_operation_canceled() {
        let status = tonic::Status::internal("operation was canceled");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_internal_with_grpc_status() {
        let status = tonic::Status::internal("grpc-status header missing");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_internal_with_compression() {
        let status = tonic::Status::internal("received message with invalid compression flag");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_internal_with_connection() {
        let status = tonic::Status::internal("error reading a body from connection");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_internal_with_transport() {
        let status = tonic::Status::internal("transport error");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_cancelled() {
        let status = tonic::Status::cancelled("operation was canceled");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_unknown() {
        let status = tonic::Status::unknown("http2 error");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_internal_unrelated_message() {
        let status = tonic::Status::internal("some other error");
        assert!(!is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_ok_status() {
        let status = tonic::Status::ok("success");
        assert!(!is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_not_found() {
        let status = tonic::Status::not_found("resource not found");
        assert!(!is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_permission_denied() {
        let status = tonic::Status::permission_denied("access denied");
        assert!(!is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_tonic_reset_error_unauthenticated() {
        let status = tonic::Status::unauthenticated("not authenticated");
        assert!(!is_tonic_reset_error(&status));
    }

    /// A reset marker under a code that does not report resets is not a reset: the
    /// message alone never makes an error retryable.
    #[test]
    fn test_is_tonic_reset_error_non_reset_code_with_marker() {
        for status in [
            tonic::Status::not_found("transport error"),
            tonic::Status::permission_denied("http2 error"),
            tonic::Status::invalid_argument("transport error"),
            tonic::Status::unavailable("transport error"),
        ] {
            assert!(
                !is_tonic_reset_error(&status),
                "{:?} must stay non-retryable",
                status.code()
            );
        }
    }

    #[test]
    fn test_is_tonic_reset_error_case_insensitive() {
        let status = tonic::Status::internal("HTTP2 ERROR OCCURRED");
        assert!(is_tonic_reset_error(&status));
    }

    #[test]
    fn test_is_connection_reset_generic_error_with_tonic_status() {
        let status = tonic::Status::internal("http2 error");
        let error: GenericError = Box::new(status);
        assert!(is_connection_reset_generic_error(&error));
    }

    #[test]
    fn test_is_connection_reset_generic_error_non_tonic() {
        let error: GenericError = Box::new(std::io::Error::other("some io error"));
        assert!(!is_connection_reset_generic_error(&error));
    }

    #[test]
    fn test_is_connection_reset_generic_error_string() {
        let error: GenericError = "simple string error".into();
        assert!(!is_connection_reset_generic_error(&error));
    }

    #[test]
    fn test_sql_flight_client_new() {
        use tonic::transport::channel::Endpoint;

        // We can't actually connect, but we can create an endpoint
        let _endpoint = Endpoint::from_static("http://localhost:50051");
        // This would fail at connect time, but the SqlFlightClient::new just takes a channel
        // So we test the construction logic indirectly
    }

    /// A status that reached the SDK only as text carries no type to classify. Under
    /// this crate's `arrow-flight` range the Flight SQL client keeps it typed, so nothing
    /// produces this shape; a build against an `arrow-flight` that erases the type would
    /// not retry it.
    /// A reset that lands on the handshake is the same transient fault as one on the
    /// query, and the handshake runs before every query an api key is configured for.
    #[test]
    fn test_handshake_reset_retries() {
        let error: GenericError = Box::new(HandshakeError {
            source: tonic::Status::unknown("transport error"),
        });
        assert!(is_connection_reset_generic_error(&error));
    }

    /// A rejected credential is permanent. Retrying it would turn one bad key into a
    /// stream of handshakes against the server.
    #[test]
    fn test_handshake_auth_failure_does_not_retry() {
        for status in [
            tonic::Status::unauthenticated("invalid api key"),
            tonic::Status::permission_denied("app is not visible to this key"),
        ] {
            let code = status.code();
            let error: GenericError = Box::new(HandshakeError { source: status });
            assert!(
                !is_connection_reset_generic_error(&error),
                "{code:?} must stay permanent"
            );
        }
    }

    /// The failure has to reach callers with the status still typed: rendering it is
    /// what makes a transient fault indistinguishable from a permanent one.
    #[tokio::test]
    async fn test_handshake_failure_preserves_the_typed_status() {
        use tonic::transport::channel::Endpoint;

        let client = SqlFlightClient::new(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            Some("an-api-key".to_string()),
            None,
            None,
            1,
        );

        let error = client
            .query("SELECT 1")
            .await
            .err()
            .expect("a refused connection fails the handshake");
        let handshake = error
            .downcast_ref::<HandshakeError>()
            .expect("the handshake failure must keep its status typed");
        assert_eq!(handshake.source.code(), tonic::Code::Unavailable);
    }

    #[test]
    fn test_handshake_error_keeps_its_context_and_status() {
        let error = HandshakeError {
            source: tonic::Status::unknown("transport error"),
        };
        let rendered = error.to_string();
        assert!(rendered.starts_with("Can't handshake: "), "{rendered}");
        assert!(rendered.contains("transport error"), "{rendered}");
    }

    /// The renewal has to key on the code alone: the runtime phrases the rejection
    /// differently for a missing header, a malformed one and an unknown session.
    #[test]
    fn test_unauthenticated_is_recognised_under_both_error_shapes() {
        for message in [
            "Missing authorization header",
            "Invalid authorization header",
            "Invalid credentials",
        ] {
            let plain: GenericError = Box::new(tonic::Status::unauthenticated(message));
            assert!(is_unauthenticated(&plain), "{message}");

            let boxed: GenericError = Box::new(FlightError::Tonic(Box::new(
                tonic::Status::unauthenticated(message),
            )));
            assert!(is_unauthenticated(&boxed), "{message}");
        }
    }

    /// Anything but a refused credential must not cost a handshake, a permission
    /// failure and a handshake failure included.
    #[test]
    fn test_other_failures_are_not_unauthenticated() {
        let errors: Vec<GenericError> = vec![
            Box::new(tonic::Status::permission_denied(
                "app is not visible to this key",
            )),
            Box::new(tonic::Status::unavailable("tcp connect error")),
            Box::new(tonic::Status::invalid_argument("bad sql")),
            Box::new(FlightError::Tonic(Box::new(tonic::Status::not_found(
                "no such table",
            )))),
            Box::new(FlightError::ProtocolError(
                "Invalid credentials".to_string(),
            )),
            Box::new(HandshakeError {
                source: tonic::Status::unauthenticated("Invalid credentials"),
            }),
            "Invalid credentials".into(),
        ];
        for error in errors {
            assert!(!is_unauthenticated(&error), "{error}");
        }
    }

    fn client_with_api_key() -> SqlFlightClient {
        use tonic::transport::channel::Endpoint;

        SqlFlightClient::new(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            Some("an-api-key".to_string()),
            None,
            None,
            1,
        )
    }

    /// Forgetting a session is scoped to the token that was rejected, so a renewal
    /// another request has already completed is kept rather than discarded for a third
    /// handshake.
    #[tokio::test]
    async fn test_forget_session_keeps_a_session_another_request_renewed() {
        let client = client_with_api_key();
        let stale: Arc<str> = Arc::from("session-1");
        let renewed: Arc<str> = Arc::from("session-2");

        client.store_session(Some(Arc::clone(&stale)));
        client.forget_session(Some(&stale));
        assert_eq!(client.cached_session(), Session::Pending);

        client.store_session(Some(Arc::clone(&renewed)));
        client.forget_session(Some(&stale));
        assert_eq!(
            client.cached_session(),
            Session::Established(Some(renewed)),
            "a session renewed by another request must survive a stale rejection"
        );
    }

    /// A runtime that runs without authentication answers the handshake with no token;
    /// that answer is still a session, and must not be handshaken again per query.
    #[tokio::test]
    async fn test_a_tokenless_handshake_is_an_established_session() {
        let client = client_with_api_key();
        client.store_session(None);
        assert_eq!(client.cached_session(), Session::Established(None));
        client.forget_session(None);
        assert_eq!(client.cached_session(), Session::Pending);
    }

    /// Without an api key there is nothing to handshake with, so a rejection is not
    /// something a renewal could fix.
    #[tokio::test]
    async fn test_no_api_key_yields_no_renewable_credential() {
        use tonic::transport::channel::Endpoint;

        let client = SqlFlightClient::new(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            None,
            None,
            None,
            1,
        );
        let credential = client.authenticate().await.expect("no handshake needed");
        assert!(credential.token.is_none());
        assert!(!credential.reused);
        assert_eq!(client.cached_session(), Session::Pending);
    }

    #[test]
    fn test_status_reduced_to_text_does_not_retry() {
        let rendered = "Status { code: Unknown, message: \"transport error\", source: Some(tonic::transport::Error(Transport, hyper::Error(Io, Kind(ConnectionReset)))) }";
        let error = FlightError::Arrow(ArrowError::IpcError(rendered.to_string()));
        assert!(!is_connection_reset_flight_error(&error));

        let error: GenericError = Box::new(ArrowError::IpcError(rendered.to_string()));
        assert!(!is_connection_reset_generic_error(&error));
    }

    /// The Flight SQL client boxes a typed status inside a `FlightError`, which the
    /// `tonic::Status` downcast alone does not reach.
    #[test]
    fn test_boxed_flight_error_tonic_retries() {
        let error: GenericError = Box::new(FlightError::Tonic(Box::new(tonic::Status::unknown(
            "transport error",
        ))));
        assert!(is_connection_reset_generic_error(&error));
    }

    #[test]
    fn test_typed_retryable_metadata_marker_retries() {
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert(
            RETRYABLE_METADATA_KEY,
            "true".parse().expect("header value"),
        );
        let status =
            tonic::Status::with_metadata(tonic::Code::Aborted, "upstream restarting", metadata);
        let error: GenericError = Box::new(FlightError::Tonic(Box::new(status)));
        assert!(is_connection_reset_generic_error(&error));
    }

    #[test]
    fn test_unrelated_ipc_error_does_not_retry() {
        let error = FlightError::Arrow(ArrowError::IpcError(
            "Unable to get root as message: invalid flatbuffer".to_string(),
        ));
        assert!(!is_connection_reset_flight_error(&error));
    }

    #[test]
    fn test_flight_protocol_error_with_marker_does_not_retry() {
        let error = FlightError::ProtocolError("transport error".to_string());
        assert!(!is_connection_reset_flight_error(&error));
    }

    /// A refused connection is a transport failure, but not the reset shape: tonic
    /// reports it as `Unavailable`, which gRPC callers already retry themselves.
    #[test]
    fn test_connect_error_does_not_retry() {
        let error: GenericError = Box::new(FlightError::Tonic(Box::new(
            tonic::Status::unavailable("tcp connect error"),
        )));
        assert!(!is_connection_reset_generic_error(&error));
    }

    /// The classification has to reach the state machine, not just the predicate: a reset
    /// yields `ConnectionReset` and re-arms the query, where an unrelated error terminates
    /// the stream.
    #[tokio::test]
    async fn test_reset_drives_the_retry_path() {
        use futures::StreamExt;
        use tonic::transport::channel::Endpoint;

        let failing = stream::iter(vec![Err(FlightError::Tonic(Box::new(
            tonic::Status::unknown("transport error"),
        )))]);
        let client = Arc::new(SqlFlightClient::new(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            None,
            None,
            None,
            1,
        ));
        let mut retryable = RetryableQueryStream::new(
            client,
            "SELECT 1",
            None,
            Box::pin(FlightRecordBatchStream::new_from_flight_data(failing)),
        );

        let first = retryable.next().await.expect("an item");
        assert!(
            matches!(first, Err(SpiceClientError::ConnectionReset { .. })),
            "expected a retryable ConnectionReset, got {first:?}"
        );
    }

    #[tokio::test]
    async fn test_unrelated_stream_error_terminates_without_retrying() {
        use futures::StreamExt;
        use tonic::transport::channel::Endpoint;

        let failing = stream::iter(vec![Err(FlightError::Arrow(ArrowError::IpcError(
            "Unable to get root as message: invalid flatbuffer".to_string(),
        )))]);
        let client = Arc::new(SqlFlightClient::new(
            Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
            None,
            None,
            None,
            1,
        ));
        let mut retryable = RetryableQueryStream::new(
            client,
            "SELECT 1",
            None,
            Box::pin(FlightRecordBatchStream::new_from_flight_data(failing)),
        );

        let first = retryable.next().await.expect("an item");
        assert!(
            matches!(first, Err(SpiceClientError::QueryStream { .. })),
            "expected a terminal QueryStream error, got {first:?}"
        );
        assert!(
            retryable.next().await.is_none(),
            "the stream must be terminated, not retried"
        );
    }
}
