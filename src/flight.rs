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
use std::collections::HashMap;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use tonic::IntoRequest;
use tonic::metadata::AsciiMetadataKey;
use tonic::transport::Channel;

#[derive(Clone)]
pub struct SqlFlightClient {
    headers: Arc<HashMap<String, String>>,
    client: FlightServiceClient<Channel>,
    api_key: Option<Arc<str>>,
    max_retries: u32,
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
        }
    }

    async fn handshake(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<String>, ArrowError> {
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
            .map_err(|e| ArrowError::IpcError(format!("Can't handshake {e}")))?;

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

    async fn authenticate(&self) -> std::result::Result<Option<String>, GenericError> {
        let (username, password) = match &self.api_key {
            Some(api_key) => ("", api_key.as_ref()),
            None => return Ok(None),
        };

        let token = self.handshake(username, password).await?;

        Ok(token)
    }

    fn set_request_headers<T>(
        &self,
        mut req: tonic::Request<T>,
        token: Option<String>,
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
        let token = self.authenticate().await?;

        let descriptor = FlightDescriptor::new_cmd(query.to_string());
        let req = self.set_request_headers(descriptor.into_request(), token.clone())?;

        let info = self.client.clone().get_flight_info(req).await?.into_inner();

        for ep in info.endpoint {
            if let Some(tkt) = ep.ticket {
                let req = tkt.into_request();
                let req = self.set_request_headers(req, token.clone())?;
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
        if let Some(params) = params {
            Ok(self.execute_prepared_statement(query, params).await?)
        } else {
            Ok(self.query(query).await?)
        }
    }

    async fn execute_prepared_statement(
        &self,
        query: &str,
        parameters: RecordBatch,
    ) -> std::result::Result<FlightRecordBatchStream, GenericError> {
        let mut client = FlightSqlServiceClient::new_from_inner(self.client.clone());
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
