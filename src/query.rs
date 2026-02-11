//! Async query API for submitting and managing long-running queries.
//!
//! This module provides the [`QueryJob`] type for managing asynchronous SQL queries
//! via the `/v1/queries` HTTP API.
//!
//! # Example
//!
//! ```no_run
//! use spiceai::{Client, ClientBuilder};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let client = ClientBuilder::new()
//!         .http_url("http://localhost:8090")
//!         .build()
//!         .await?;
//!
//!     // Submit an async query
//!     let job = client.query("SELECT * FROM large_table").await?;
//!     println!("Submitted query: {}", job.id());
//!
//!     // Wait for completion
//!     let result = job.wait().await?;
//!     println!("Query completed with {} rows", result.total_rows);
//!
//!     // Get results as record batches
//!     let batches = job.results().await?;
//!     for batch in batches {
//!         println!("Got {} rows", batch.num_rows());
//!     }
//!
//!     Ok(())
//! }
//! ```

use arrow::array::RecordBatch;
use arrow::ipc::reader::StreamReader;
use futures::Stream;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

/// Default poll interval for checking query status.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Errors that can occur during async query operations.
#[derive(Debug, Snafu)]
pub enum QueryError {
    /// Failed to submit the query.
    #[snafu(display("Failed to submit query (HTTP {status_code}): {response_body}"))]
    SubmitFailed {
        /// HTTP status code returned by the server.
        status_code: u16,
        /// Response body from the server.
        response_body: String,
    },

    /// Query was not found on the server.
    #[snafu(display("Query not found: {query_id}"))]
    NotFound { query_id: String },

    /// Query results have expired or been cleaned up.
    #[snafu(display("Query results expired: {query_id}"))]
    Expired { query_id: String },

    /// Query is not yet complete.
    #[snafu(display("Query not yet complete: {query_id}"))]
    NotReady { query_id: String },

    /// Query execution failed on the server.
    #[snafu(display("Query failed: {message}"))]
    ExecutionFailed { message: String },

    /// Query was cancelled.
    #[snafu(display("Query was cancelled: {query_id}"))]
    Cancelled { query_id: String },

    /// HTTP request failed with an error response.
    #[snafu(display("HTTP request failed (HTTP {status_code}): {response_body}"))]
    HttpRequestFailed {
        /// HTTP status code returned by the server.
        status_code: u16,
        /// Response body from the server.
        response_body: String,
    },

    /// HTTP transport error.
    #[snafu(display("HTTP request failed: {message}"))]
    HttpError { message: String },

    /// Failed to parse server response.
    #[snafu(display("Failed to parse response: {message}"))]
    ParseError { message: String },

    /// Async queries require cluster mode.
    #[snafu(display(
        "Async queries require cluster mode with scheduler.state_location configured"
    ))]
    ClusterModeRequired,

    /// Timeout waiting for query to complete.
    #[snafu(display("Timeout waiting for query {query_id} to complete"))]
    Timeout { query_id: String },

    /// Failed to deserialize Arrow IPC data.
    #[snafu(display("Failed to deserialize Arrow data: {message}"))]
    ArrowError { message: String },
}

/// The current status of an async query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum QueryStatus {
    /// Query is queued but not yet running.
    Pending,
    /// Query is actively executing.
    Running,
    /// Query completed successfully, results available.
    Succeeded,
    /// Query execution failed.
    Failed,
    /// Query was cancelled by user.
    Cancelled,
    /// Query results have been cleaned up / expired.
    Closed,
}

impl QueryStatus {
    /// Returns `true` if the query has completed successfully.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }

    /// Returns `true` if the query has failed.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed)
    }

    /// Returns `true` if the query was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Returns `true` if the query is still running or pending.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    /// Returns `true` if the query has reached a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Closed
        )
    }
}

impl std::fmt::Display for QueryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "PENDING"),
            Self::Running => write!(f, "RUNNING"),
            Self::Succeeded => write!(f, "SUCCEEDED"),
            Self::Failed => write!(f, "FAILED"),
            Self::Cancelled => write!(f, "CANCELLED"),
            Self::Closed => write!(f, "CLOSED"),
        }
    }
}

/// Information about a completed query result.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Total number of rows in the result.
    pub total_rows: u64,
    /// Total number of chunks/partitions.
    pub total_chunks: u64,
}

/// Detailed status information for a query.
#[derive(Debug, Clone)]
pub struct QueryInfo {
    /// The query ID.
    pub query_id: String,
    /// Current status.
    pub status: QueryStatus,
    /// Error details if the query failed.
    pub error: Option<QueryErrorInfo>,
    /// Result metadata if completed.
    pub result: Option<QueryResult>,
}

/// Error information for a failed query.
#[derive(Debug, Clone)]
pub struct QueryErrorInfo {
    /// Error code.
    pub error_code: String,
    /// Error message.
    pub message: String,
}

/// Summary of a query for listing.
#[derive(Debug, Clone)]
pub struct QuerySummary {
    /// The query ID.
    pub query_id: String,
    /// Current status of the query.
    pub status: QueryStatus,
    /// When the query was created.
    pub created_at: String,
    /// Preview of the SQL query (may be truncated).
    pub sql_preview: String,
}

/// Response from listing queries.
#[derive(Debug, Clone)]
pub struct QueryListResponse {
    /// List of queries.
    pub queries: Vec<QuerySummary>,
    /// Total count of queries matching the filter.
    pub total_count: Option<usize>,
}

/// Represents the current state of the `QueryResultStream` state machine.
enum ResultStreamState {
    /// Ready to fetch the next chunk. Contains the chunk index to fetch after this one completes.
    FetchingChunk {
        future:
            Pin<Box<dyn std::future::Future<Output = Result<Vec<RecordBatch>, QueryError>> + Send>>,
        next_chunk_after: u64,
    },
    /// Yielding batches from the current chunk.
    YieldingBatches {
        batches: std::vec::IntoIter<RecordBatch>,
        next_chunk: u64,
    },
    /// Stream has completed.
    Completed,
}

/// A stream of `RecordBatch` results from an async query.
///
/// This stream fetches result chunks lazily, yielding record batches as they are
/// retrieved from the server. This avoids loading all results into memory at once.
///
/// # Example
///
/// ```no_run
/// use futures::StreamExt;
/// use spiceai::ClientBuilder;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let client = ClientBuilder::new()
///     .http_url("http://localhost:8090")
///     .build()
///     .await?;
///
/// let job = client.query("SELECT * FROM large_table").await?;
/// job.wait().await?;
///
/// // Stream results without loading all into memory
/// let mut stream = job.results_stream().await?;
/// while let Some(result) = stream.next().await {
///     let batch = result?;
///     println!("Got batch with {} rows", batch.num_rows());
/// }
/// # Ok(())
/// # }
/// ```
pub struct QueryResultStream {
    client: Arc<QueryHttpClient>,
    query_id: String,
    total_chunks: u64,
    state: ResultStreamState,
}

impl QueryResultStream {
    fn new(client: Arc<QueryHttpClient>, query_id: String, total_chunks: u64) -> Self {
        // Start by yielding from an empty iterator, which will trigger fetching chunk 0
        Self {
            client,
            query_id,
            total_chunks,
            state: ResultStreamState::YieldingBatches {
                batches: Vec::new().into_iter(),
                next_chunk: 0,
            },
        }
    }
}

impl Stream for QueryResultStream {
    type Item = Result<RecordBatch, QueryError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match &mut self.state {
                ResultStreamState::FetchingChunk {
                    future,
                    next_chunk_after,
                } => match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(batches)) => {
                        let next_chunk = *next_chunk_after;
                        self.state = ResultStreamState::YieldingBatches {
                            batches: batches.into_iter(),
                            next_chunk,
                        };
                    }
                    Poll::Ready(Err(e)) => {
                        self.state = ResultStreamState::Completed;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => return Poll::Pending,
                },
                ResultStreamState::YieldingBatches {
                    batches,
                    next_chunk,
                } => {
                    if let Some(batch) = batches.next() {
                        return Poll::Ready(Some(Ok(batch)));
                    }
                    // Current chunk exhausted, fetch next if available
                    let chunk_to_fetch = *next_chunk;
                    if chunk_to_fetch >= self.total_chunks {
                        self.state = ResultStreamState::Completed;
                        return Poll::Ready(None);
                    }
                    let client = Arc::clone(&self.client);
                    let query_id = self.query_id.clone();
                    #[allow(clippy::cast_possible_truncation)]
                    let fut = Box::pin(async move {
                        client
                            .get_results_arrow(&query_id, chunk_to_fetch as usize)
                            .await
                    });
                    self.state = ResultStreamState::FetchingChunk {
                        future: fut,
                        next_chunk_after: chunk_to_fetch + 1,
                    };
                }
                ResultStreamState::Completed => return Poll::Ready(None),
            }
        }
    }
}

/// HTTP client configuration for async queries.
#[derive(Clone)]
pub(crate) struct QueryHttpClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl QueryHttpClient {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    fn add_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.header("X-API-Key", key),
            None => req,
        }
    }

    pub async fn submit(&self, sql: &str) -> Result<SubmitResponse, QueryError> {
        let url = format!("{}/v1/queries", self.base_url);
        let body = SubmitRequest {
            sql: sql.to_string(),
            parameters: None,
            timeout_seconds: None,
            maximum_size: None,
        };

        let response = self
            .add_auth(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            202 => response.json().await.map_err(|e| QueryError::ParseError {
                message: e.to_string(),
            }),
            503 => Err(QueryError::ClusterModeRequired),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::SubmitFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }

    pub async fn get_status(&self, query_id: &str) -> Result<StatusResponse, QueryError> {
        let url = format!("{}/v1/queries/{}/status", self.base_url, query_id);

        let response = self
            .add_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => response.json().await.map_err(|e| QueryError::ParseError {
                message: e.to_string(),
            }),
            404 => Err(QueryError::NotFound {
                query_id: query_id.to_string(),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::HttpRequestFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }

    pub async fn get_query(&self, query_id: &str) -> Result<QueryInfoResponse, QueryError> {
        let url = format!("{}/v1/queries/{}", self.base_url, query_id);

        let response = self
            .add_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => response.json().await.map_err(|e| QueryError::ParseError {
                message: e.to_string(),
            }),
            404 => Err(QueryError::NotFound {
                query_id: query_id.to_string(),
            }),
            410 => Err(QueryError::Expired {
                query_id: query_id.to_string(),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::HttpRequestFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }

    #[allow(dead_code)]
    pub async fn get_results(
        &self,
        query_id: &str,
        chunk_index: usize,
    ) -> Result<ResultChunkResponse, QueryError> {
        let url = format!(
            "{}/v1/queries/{}/results/chunks/{}",
            self.base_url, query_id, chunk_index
        );

        let response = self
            .add_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => response.json().await.map_err(|e| QueryError::ParseError {
                message: e.to_string(),
            }),
            404 => Err(QueryError::NotFound {
                query_id: query_id.to_string(),
            }),
            410 => Err(QueryError::Expired {
                query_id: query_id.to_string(),
            }),
            409 | 425 => Err(QueryError::NotReady {
                query_id: query_id.to_string(),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::HttpRequestFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }

    pub async fn get_results_arrow(
        &self,
        query_id: &str,
        chunk_index: usize,
    ) -> Result<Vec<RecordBatch>, QueryError> {
        let url = format!(
            "{}/v1/queries/{}/results/chunks/{}?format=arrow",
            self.base_url, query_id, chunk_index
        );

        let response = self
            .add_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => {
                let bytes = response.bytes().await.map_err(|e| QueryError::HttpError {
                    message: e.to_string(),
                })?;
                parse_arrow_ipc(&bytes)
            }
            404 => Err(QueryError::NotFound {
                query_id: query_id.to_string(),
            }),
            410 => Err(QueryError::Expired {
                query_id: query_id.to_string(),
            }),
            409 | 425 => Err(QueryError::NotReady {
                query_id: query_id.to_string(),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::HttpRequestFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }

    pub async fn cancel(&self, query_id: &str) -> Result<QueryInfoResponse, QueryError> {
        let url = format!("{}/v1/queries/{}/cancel", self.base_url, query_id);

        let response = self
            .add_auth(self.client.post(&url))
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => response.json().await.map_err(|e| QueryError::ParseError {
                message: e.to_string(),
            }),
            404 => Err(QueryError::NotFound {
                query_id: query_id.to_string(),
            }),
            409 => Err(QueryError::HttpRequestFailed {
                status_code: 409,
                response_body: format!("Query {query_id} has already completed"),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::HttpRequestFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }

    /// List queries with optional status filter and limit.
    pub async fn list_queries(
        &self,
        status_filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<QueryListResponse, QueryError> {
        let mut url = format!("{}/v1/queries", self.base_url);

        let mut params = Vec::new();
        if let Some(status) = status_filter {
            params.push(format!("status={status}"));
        }
        if let Some(limit) = limit {
            params.push(format!("limit={limit}"));
        }
        if !params.is_empty() {
            url = format!("{url}?{}", params.join("&"));
        }

        let response = self
            .add_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| QueryError::HttpError {
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => {
                let list_response: ListQueriesApiResponse =
                    response.json().await.map_err(|e| QueryError::ParseError {
                        message: e.to_string(),
                    })?;

                Ok(QueryListResponse {
                    queries: list_response
                        .queries
                        .into_iter()
                        .map(|q| QuerySummary {
                            query_id: q.query_id,
                            status: q.status,
                            created_at: q.created_at,
                            sql_preview: q.sql_preview,
                        })
                        .collect(),
                    total_count: list_response.total_count,
                })
            }
            503 => Err(QueryError::ClusterModeRequired),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(QueryError::HttpRequestFailed {
                    status_code,
                    response_body,
                })
            }
        }
    }
}

/// Parse Arrow IPC stream data into record batches.
fn parse_arrow_ipc(data: &[u8]) -> Result<Vec<RecordBatch>, QueryError> {
    let cursor = Cursor::new(data);
    let reader = StreamReader::try_new(cursor, None).map_err(|e| QueryError::ArrowError {
        message: e.to_string(),
    })?;

    let mut batches = Vec::new();
    for batch_result in reader {
        let batch = batch_result.map_err(|e| QueryError::ArrowError {
            message: e.to_string(),
        })?;
        batches.push(batch);
    }
    Ok(batches)
}

// API request/response types

#[derive(Debug, Serialize)]
struct SubmitRequest {
    sql: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum_size: Option<u64>,
}

/// Maps to `SubmitQueryResponse` from the API.
#[derive(Debug, Deserialize)]
pub struct SubmitResponse {
    pub query_id: String,
    #[allow(dead_code)]
    pub status: QueryStatus,
    #[allow(dead_code)]
    #[serde(default)]
    pub error: Option<ErrorResponse>,
    #[allow(dead_code)]
    pub status_url: String,
    #[allow(dead_code)]
    pub results_url: String,
}

/// Maps to `StatusResponse` from the API (`GET /v1/queries/{id}/status`).
#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    pub status: QueryStatus,
    #[serde(default)]
    pub error: Option<ErrorResponse>,
}

/// Maps to `ErrorResponse` from the API.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorResponse {
    #[serde(default)]
    pub error_code: String,
    pub message: String,
    #[serde(default)]
    pub sql_state: Option<String>,
}

/// Maps to `QueryResponse` from the API (`GET /v1/queries/{id}`).
#[derive(Debug, Deserialize)]
pub struct QueryInfoResponse {
    pub query_id: String,
    pub status: QueryStatus,
    #[serde(default)]
    pub error: Option<ErrorResponse>,
    #[serde(default)]
    pub manifest: Option<ManifestMetadata>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Maps to `ManifestResponse` from the API.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestMetadata {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
    pub total_row_count: u64,
    pub total_chunk_count: u64,
}

/// Maps to `ChunkResponse` from the API.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ResultChunkResponse {
    pub chunk_index: usize,
    pub row_offset: usize,
    pub row_count: usize,
    #[serde(default)]
    pub next_chunk_index: Option<usize>,
    #[serde(default)]
    pub next_chunk_url: Option<String>,
    #[serde(default)]
    pub data_array: Option<Vec<serde_json::Value>>,
}

/// Maps to `ListQueriesResponse` from the API.
#[derive(Debug, Deserialize)]
pub struct ListQueriesApiResponse {
    pub queries: Vec<QuerySummaryApiResponse>,
    #[serde(default)]
    pub total_count: Option<usize>,
}

/// Maps to `QuerySummary` from the API.
#[derive(Debug, Deserialize)]
pub struct QuerySummaryApiResponse {
    pub query_id: String,
    pub status: QueryStatus,
    pub created_at: String,
    pub sql_preview: String,
}

/// A handle to an async query job.
///
/// `QueryJob` provides methods to check the status of a query, wait for completion,
/// retrieve results, and cancel the query.
///
/// # Example
///
/// ```no_run
/// # use spiceai::{Client, ClientBuilder};
/// # #[tokio::main]
/// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let client = ClientBuilder::new()
///     .http_url("http://localhost:8090")
///     .build()
///     .await?;
///
/// let job = client.query("SELECT * FROM users").await?;
///
/// // Check status
/// let status = job.status().await?;
/// println!("Status: {}", status);
///
/// // Wait for completion with timeout
/// let result = job.wait_timeout(std::time::Duration::from_secs(60)).await?;
///
/// // Get results as Arrow record batches
/// let batches = job.results().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct QueryJob {
    query_id: String,
    client: Arc<QueryHttpClient>,
    poll_interval: Duration,
}

impl QueryJob {
    pub(crate) fn new(query_id: String, client: Arc<QueryHttpClient>) -> Self {
        Self {
            query_id,
            client,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    /// Returns the query ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.query_id
    }

    /// Sets the poll interval for waiting operations.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Gets the current status of the query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query is not found or the HTTP request fails.
    pub async fn status(&self) -> Result<QueryStatus, QueryError> {
        let response = self.client.get_status(&self.query_id).await?;
        Ok(response.status)
    }

    /// Gets detailed information about the query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query is not found or the HTTP request fails.
    pub async fn info(&self) -> Result<QueryInfo, QueryError> {
        let response = self.client.get_query(&self.query_id).await?;
        Ok(QueryInfo {
            query_id: response.query_id,
            status: response.status,
            error: response.error.map(|e| QueryErrorInfo {
                error_code: e.error_code,
                message: e.message,
            }),
            result: response.manifest.map(|r| QueryResult {
                total_rows: r.total_row_count,
                total_chunks: r.total_chunk_count,
            }),
        })
    }

    /// Waits for the query to complete (success, failure, or cancellation).
    ///
    /// This method polls the query status until it reaches a terminal state.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or is cancelled.
    pub async fn wait(&self) -> Result<QueryResult, QueryError> {
        self.wait_with_options(None).await
    }

    /// Waits for the query to complete with a timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails, is cancelled, or the timeout is reached.
    pub async fn wait_timeout(&self, timeout: Duration) -> Result<QueryResult, QueryError> {
        self.wait_with_options(Some(timeout)).await
    }

    async fn wait_with_options(
        &self,
        timeout: Option<Duration>,
    ) -> Result<QueryResult, QueryError> {
        let start = std::time::Instant::now();

        loop {
            let info = self.info().await?;

            match info.status {
                QueryStatus::Succeeded => {
                    return info.result.ok_or_else(|| QueryError::ParseError {
                        message: "Query succeeded but no result metadata".to_string(),
                    });
                }
                QueryStatus::Failed => {
                    let message = info
                        .error
                        .map_or_else(|| "Unknown error".to_string(), |e| e.message);
                    return Err(QueryError::ExecutionFailed { message });
                }
                QueryStatus::Cancelled => {
                    return Err(QueryError::Cancelled {
                        query_id: self.query_id.clone(),
                    });
                }
                QueryStatus::Closed => {
                    return Err(QueryError::Expired {
                        query_id: self.query_id.clone(),
                    });
                }
                QueryStatus::Pending | QueryStatus::Running => {
                    // Check timeout
                    if timeout.is_some_and(|t| start.elapsed() >= t) {
                        return Err(QueryError::Timeout {
                            query_id: self.query_id.clone(),
                        });
                    }
                    tokio::time::sleep(self.poll_interval).await;
                }
            }
        }
    }

    /// Retrieves the results of a completed query as Arrow record batches.
    ///
    /// This method fetches all result chunks and returns them as a vector of `RecordBatch`.
    ///
    /// # Errors
    ///
    /// Returns an error if the query is not complete, not found, or results have expired.
    pub async fn results(&self) -> Result<Vec<RecordBatch>, QueryError> {
        use futures::TryStreamExt;
        let stream = self.results_stream().await?;
        stream.try_collect().await
    }

    /// Returns a stream of `RecordBatch` results from a completed query.
    ///
    /// This method returns a stream that fetches result chunks lazily, yielding
    /// record batches as they are retrieved from the server. This avoids loading
    /// all results into memory at once, making it suitable for large result sets.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use futures::StreamExt;
    /// use spiceai::ClientBuilder;
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let job = client.query("SELECT * FROM large_table").await?;
    /// job.wait().await?;
    ///
    /// // Stream results without loading all into memory
    /// let mut stream = job.results_stream().await?;
    /// while let Some(result) = stream.next().await {
    ///     let batch = result?;
    ///     println!("Got batch with {} rows", batch.num_rows());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the query is not complete, not found, or results have expired.
    pub async fn results_stream(&self) -> Result<QueryResultStream, QueryError> {
        let info = self.info().await?;

        if !info.status.is_success() {
            return Err(QueryError::NotReady {
                query_id: self.query_id.clone(),
            });
        }

        let total_chunks = info.result.map_or(1, |r| r.total_chunks);

        Ok(QueryResultStream::new(
            Arc::clone(&self.client),
            self.query_id.clone(),
            total_chunks,
        ))
    }

    /// Cancels the query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query is not found or has already completed.
    pub async fn cancel(&self) -> Result<QueryInfo, QueryError> {
        let response = self.client.cancel(&self.query_id).await?;
        Ok(QueryInfo {
            query_id: response.query_id,
            status: response.status,
            error: response.error.map(|e| QueryErrorInfo {
                error_code: e.error_code,
                message: e.message,
            }),
            result: response.manifest.map(|r| QueryResult {
                total_rows: r.total_row_count,
                total_chunks: r.total_chunk_count,
            }),
        })
    }
}

impl std::fmt::Debug for QueryJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryJob")
            .field("query_id", &self.query_id)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}
