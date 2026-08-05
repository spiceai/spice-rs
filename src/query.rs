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
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

use crate::dataset::{DatasetError, DatasetRefreshRequest, DatasetRefreshResponse};
use crate::params::{QueryParameterError, QueryParameters};
use crate::search::{SearchError, SearchRequest, SearchResponse};
use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use futures::Stream;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
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

    /// Failed to deserialize Arrow data from server response.
    #[snafu(display("Failed to deserialize Arrow data: {message}"))]
    ArrowError { message: String },

    /// A bind parameter could not be encoded for the async `/v1/queries` API.
    #[snafu(display("Invalid query parameter: {source}"))]
    InvalidParameter {
        /// The underlying parameter conversion error.
        source: QueryParameterError,
    },
}

/// Options for submitting an async query via
/// [`SpiceClient::query_with_options`](crate::Client::query_with_options).
///
/// Construct with [`QuerySubmitOptions::new`] and chain the builder methods.
/// Unset fields are omitted from the request, letting the server apply its
/// defaults.
///
/// ```
/// use spiceai::{QueryParameters, QuerySubmitOptions};
///
/// let options = QuerySubmitOptions::new()
///     .bindings(QueryParameters::new().push("active"))
///     .timeout_seconds(300)
///     .maximum_size(100_000_000);
/// ```
#[derive(Debug, Clone, Default)]
pub struct QuerySubmitOptions {
    pub(crate) bindings: Option<QueryParameters>,
    pub(crate) timeout_seconds: Option<u64>,
    pub(crate) maximum_size: Option<u64>,
}

impl QuerySubmitOptions {
    /// Creates an empty set of submit options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets positional scalar bind parameters (`$1`, `$2`, ...) for the query.
    #[must_use]
    pub fn bindings(mut self, params: impl Into<QueryParameters>) -> Self {
        self.bindings = Some(params.into());
        self
    }

    /// Sets the maximum execution time, in seconds, before the server aborts
    /// the query.
    #[must_use]
    pub fn timeout_seconds(mut self, seconds: u64) -> Self {
        self.timeout_seconds = Some(seconds);
        self
    }

    /// Sets the maximum materialized result size, in bytes.
    #[must_use]
    pub fn maximum_size(mut self, bytes: u64) -> Self {
        self.maximum_size = Some(bytes);
        self
    }
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
/// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    schema: SchemaRef,
    state: ResultStreamState,
}

impl QueryResultStream {
    fn new(
        client: Arc<QueryHttpClient>,
        query_id: String,
        total_chunks: u64,
        schema: SchemaRef,
    ) -> Self {
        // Start by yielding from an empty iterator, which will trigger fetching chunk 0
        Self {
            client,
            query_id,
            total_chunks,
            schema,
            state: ResultStreamState::YieldingBatches {
                batches: Vec::new().into_iter(),
                next_chunk: 0,
            },
        }
    }

    /// Creates a stream that yields a single empty batch and then completes.
    ///
    /// Used when the query returned 0 rows: the chunk API will 404, so we
    /// construct an empty `RecordBatch` with the correct schema from the
    /// manifest and return it immediately.
    fn empty_with_schema(
        client: Arc<QueryHttpClient>,
        query_id: String,
        schema: SchemaRef,
    ) -> Self {
        let empty_batch = RecordBatch::new_empty(Arc::clone(&schema));
        Self {
            client,
            query_id,
            total_chunks: 0,
            schema,
            state: ResultStreamState::YieldingBatches {
                batches: vec![empty_batch].into_iter(),
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
                    let schema = Arc::clone(&self.schema);
                    #[allow(clippy::cast_possible_truncation)]
                    let fut = Box::pin(async move {
                        client
                            .get_results_arrow(&query_id, chunk_to_fetch as usize, &schema)
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
    /// Builds a client carrying the same-origin redirect policy (#12502), for tests that need
    /// one pointed at a mock server.
    ///
    /// Test-only: in production the sole construction path is [`Self::with_client`], fed by
    /// `SpiceClientBuilder::build`. Keeping it that way is what makes the policy impossible to
    /// miss, so this is gated rather than left as an unused second door into the type.
    ///
    /// # Errors
    ///
    /// Returns an error if the TLS backend cannot be initialised. The policy is the reason
    /// this is fallible where `reqwest::Client::new` — the call it replaces — panicked: a
    /// client built by defaulting past the failure would silently not carry it.
    #[cfg(test)]
    pub fn new(base_url: &str, api_key: Option<String>) -> Result<Self, reqwest::Error> {
        let client = crate::redirect::credentialed_client_builder().build()?;

        Ok(Self::with_client(client, base_url, api_key))
    }

    pub fn with_client(client: reqwest::Client, base_url: &str, api_key: Option<String>) -> Self {
        Self {
            client,
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

    pub async fn submit(
        &self,
        sql: &str,
        parameters: Option<serde_json::Value>,
        timeout_seconds: Option<u64>,
        maximum_size: Option<u64>,
    ) -> Result<SubmitResponse, QueryError> {
        let url = format!("{}/v1/queries", self.base_url);
        let body = SubmitRequest {
            sql: sql.to_string(),
            parameters,
            timeout_seconds,
            maximum_size,
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
        schema: &SchemaRef,
    ) -> Result<Vec<RecordBatch>, QueryError> {
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
            200 => {
                let chunk: ResultChunkResponse =
                    response.json().await.map_err(|e| QueryError::ParseError {
                        message: e.to_string(),
                    })?;
                json_data_array_to_batches(chunk.data_array.as_deref(), schema)
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

    pub async fn refresh_dataset(
        &self,
        dataset_name: &str,
        request: &DatasetRefreshRequest,
    ) -> Result<DatasetRefreshResponse, DatasetError> {
        let mut url = reqwest::Url::parse(&self.base_url).map_err(|e| DatasetError::HttpError {
            dataset_name: dataset_name.to_string(),
            message: e.to_string(),
        })?;
        {
            let mut path_segments =
                url.path_segments_mut()
                    .map_err(|_| DatasetError::HttpError {
                        dataset_name: dataset_name.to_string(),
                        message: "Base URL cannot be used for dataset refresh".to_string(),
                    })?;
            path_segments.push("v1");
            path_segments.push("datasets");
            path_segments.push(dataset_name);
            path_segments.push("acceleration");
            path_segments.push("refresh");
        }

        let request_builder = self.add_auth(self.client.post(url));
        let response = if request.has_overrides() {
            request_builder.json(request).send().await
        } else {
            request_builder.send().await
        }
        .map_err(|e| DatasetError::HttpError {
            dataset_name: dataset_name.to_string(),
            message: e.to_string(),
        })?;

        match response.status().as_u16() {
            200 | 201 => response.json().await.map_err(|e| DatasetError::ParseError {
                dataset_name: dataset_name.to_string(),
                message: e.to_string(),
            }),
            400 => {
                let response_body = response.text().await.unwrap_or_default();
                if response_body.contains("does not have acceleration enabled") {
                    Err(DatasetError::AccelerationNotEnabled {
                        dataset_name: dataset_name.to_string(),
                    })
                } else {
                    Err(DatasetError::RefreshFailed {
                        dataset_name: dataset_name.to_string(),
                        status_code: 400,
                        response_body,
                    })
                }
            }
            404 => Err(DatasetError::NotFound {
                dataset_name: dataset_name.to_string(),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(DatasetError::RefreshFailed {
                    dataset_name: dataset_name.to_string(),
                    status_code,
                    response_body,
                })
            }
        }
    }

    pub async fn search(&self, request: &SearchRequest) -> Result<SearchResponse, SearchError> {
        request.validate()?;

        let url = format!("{}/v1/search", self.base_url);

        let response = self
            .add_auth(self.client.post(&url))
            .json(request)
            .send()
            .await
            .map_err(|e| SearchError::HttpError {
                message: e.to_string(),
            })?;

        let status_code = response.status().as_u16();
        if status_code != 200 {
            // The runtime explains search failures in a plain-text body ("No
            // data sources provided"). Surface it, not just the status code.
            let response_body = response.text().await.unwrap_or_default();
            return Err(SearchError::SearchFailed {
                status_code,
                response_body: response_body.trim().to_string(),
            });
        }

        response.json().await.map_err(|e| SearchError::ParseError {
            message: e.to_string(),
        })
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

/// Convert the `data_array` JSON values from a chunk response into `RecordBatch`es.
///
/// The server returns each row as a JSON object. We serialize them into
/// newline-delimited JSON and use `arrow_json::ReaderBuilder` (with the
/// known schema) to reconstruct typed Arrow arrays.
fn json_data_array_to_batches(
    data_array: Option<&[serde_json::Value]>,
    schema: &SchemaRef,
) -> Result<Vec<RecordBatch>, QueryError> {
    let rows = match data_array {
        Some(rows) if !rows.is_empty() => rows,
        _ => return Ok(vec![RecordBatch::new_empty(Arc::clone(schema))]),
    };

    // Build newline-delimited JSON from all row values.
    let ndjson: String = rows
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let reader = arrow_json::ReaderBuilder::new(Arc::clone(schema))
        .build(std::io::Cursor::new(ndjson.as_bytes()))
        .map_err(|e| QueryError::ArrowError {
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

/// Parse a type name string (from the manifest) into an Arrow `DataType`.
///
/// The server serializes types via `DataType::to_string()`, so we match
/// on the resulting strings. Unknown types fall back to `Utf8`.
fn parse_type_name(type_name: &str) -> DataType {
    match type_name {
        "Null" => DataType::Null,
        "Boolean" => DataType::Boolean,
        "Int8" => DataType::Int8,
        "Int16" => DataType::Int16,
        "Int32" => DataType::Int32,
        "Int64" => DataType::Int64,
        "UInt8" => DataType::UInt8,
        "UInt16" => DataType::UInt16,
        "UInt32" => DataType::UInt32,
        "UInt64" => DataType::UInt64,
        "Float16" => DataType::Float16,
        "Float32" => DataType::Float32,
        "Float64" => DataType::Float64,
        "Utf8" => DataType::Utf8,
        "LargeUtf8" => DataType::LargeUtf8,
        "Utf8View" => DataType::Utf8View,
        "Binary" => DataType::Binary,
        "LargeBinary" => DataType::LargeBinary,
        "BinaryView" => DataType::BinaryView,
        "Date32" => DataType::Date32,
        "Date64" => DataType::Date64,
        s if s.starts_with("Decimal128") => parse_decimal128(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("Decimal256") => parse_decimal256(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("Timestamp") => parse_timestamp(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("Duration") => parse_duration(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("Time32") => parse_time32(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("Time64") => parse_time64(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("Interval") => parse_interval(s).unwrap_or(DataType::Utf8),
        s if s.starts_with("FixedSizeBinary") => {
            parse_fixed_size_binary(s).unwrap_or(DataType::Utf8)
        }
        _ => {
            tracing::warn!("Unrecognized Arrow type name '{type_name}', defaulting to Utf8");
            DataType::Utf8
        }
    }
}

/// Parse `Decimal128(precision, scale)` from a type name string.
fn parse_decimal128(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Decimal128(")?.strip_suffix(')')?;
    let (p, sc) = inner.split_once(", ")?;
    Some(DataType::Decimal128(p.parse().ok()?, sc.parse().ok()?))
}

/// Parse `Decimal256(precision, scale)` from a type name string.
fn parse_decimal256(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Decimal256(")?.strip_suffix(')')?;
    let (p, sc) = inner.split_once(", ")?;
    Some(DataType::Decimal256(p.parse().ok()?, sc.parse().ok()?))
}

/// Parse a `TimeUnit` from its Display string.
fn parse_time_unit(s: &str) -> Option<arrow::datatypes::TimeUnit> {
    match s {
        "Second" => Some(arrow::datatypes::TimeUnit::Second),
        "Millisecond" => Some(arrow::datatypes::TimeUnit::Millisecond),
        "Microsecond" => Some(arrow::datatypes::TimeUnit::Microsecond),
        "Nanosecond" => Some(arrow::datatypes::TimeUnit::Nanosecond),
        _ => None,
    }
}

/// Parse `Timestamp(unit, tz)` from a type name string.
fn parse_timestamp(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Timestamp(")?.strip_suffix(')')?;
    let (unit_str, tz_str) = inner.split_once(", ")?;
    let unit = parse_time_unit(unit_str)?;
    let tz = if tz_str == "None" {
        None
    } else {
        Some(tz_str.trim_matches('"').into())
    };
    Some(DataType::Timestamp(unit, tz))
}

/// Parse `Duration(unit)` from a type name string.
fn parse_duration(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Duration(")?.strip_suffix(')')?;
    Some(DataType::Duration(parse_time_unit(inner)?))
}

/// Parse `Time32(unit)` from a type name string.
fn parse_time32(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Time32(")?.strip_suffix(')')?;
    Some(DataType::Time32(parse_time_unit(inner)?))
}

/// Parse `Time64(unit)` from a type name string.
fn parse_time64(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Time64(")?.strip_suffix(')')?;
    Some(DataType::Time64(parse_time_unit(inner)?))
}

/// Parse `Interval(unit)` from a type name string.
fn parse_interval(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("Interval(")?.strip_suffix(')')?;
    let unit = match inner {
        "YearMonth" => arrow::datatypes::IntervalUnit::YearMonth,
        "DayTime" => arrow::datatypes::IntervalUnit::DayTime,
        "MonthDayNano" => arrow::datatypes::IntervalUnit::MonthDayNano,
        _ => return None,
    };
    Some(DataType::Interval(unit))
}

/// Parse `FixedSizeBinary(n)` from a type name string.
fn parse_fixed_size_binary(s: &str) -> Option<DataType> {
    let inner = s.strip_prefix("FixedSizeBinary(")?.strip_suffix(')')?;
    Some(DataType::FixedSizeBinary(inner.parse().ok()?))
}

/// Build an Arrow [`Schema`] from a [`ManifestSchema`].
fn schema_from_manifest(manifest_schema: &ManifestSchema) -> SchemaRef {
    let fields: Vec<Field> = manifest_schema
        .columns
        .iter()
        .map(|col| Field::new(&col.name, parse_type_name(&col.type_name), col.nullable))
        .collect();
    Arc::new(Schema::new(fields))
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
    pub schema: Option<ManifestSchema>,
    pub total_row_count: u64,
    pub total_chunk_count: u64,
}

/// Schema information from the manifest response.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestSchema {
    /// Number of columns.
    pub column_count: usize,
    /// Column definitions.
    pub columns: Vec<ManifestSchemaColumn>,
}

/// Schema information for a single column from the manifest response.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestSchemaColumn {
    /// Column name.
    pub name: String,
    /// Arrow data type name (e.g. "Int32", "Utf8", "Boolean").
    pub type_name: String,
    /// Whether the column can contain nulls.
    pub nullable: bool,
    /// Column position (0-indexed).
    pub position: usize,
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
/// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        let response = self.client.get_query(&self.query_id).await?;

        if !response.status.is_success() {
            return Err(QueryError::NotReady {
                query_id: self.query_id.clone(),
            });
        }

        let (total_rows, total_chunks, manifest_schema) = match &response.manifest {
            Some(manifest) => (
                manifest.total_row_count,
                manifest.total_chunk_count,
                manifest.schema.as_ref(),
            ),
            None => (0, 1, None),
        };

        // When the query returned zero rows the chunk retrieval API will
        // return a 404. Instead, build an empty RecordBatch that carries
        // the correct result schema so callers can still inspect columns.
        let schema =
            manifest_schema.map_or_else(|| Arc::new(Schema::empty()), schema_from_manifest);

        if total_rows == 0 {
            return Ok(QueryResultStream::empty_with_schema(
                Arc::clone(&self.client),
                self.query_id.clone(),
                schema,
            ));
        }

        Ok(QueryResultStream::new(
            Arc::clone(&self.client),
            self.query_id.clone(),
            total_chunks,
            schema,
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{IntervalUnit, TimeUnit};
    use futures::StreamExt;

    // -----------------------------------------------------------------------
    // Helper: build a QueryHttpClient pointed at `base_url`
    // -----------------------------------------------------------------------
    fn test_http_client(base_url: &str) -> Arc<QueryHttpClient> {
        Arc::new(QueryHttpClient::new(base_url, None).expect("build client"))
    }

    // -----------------------------------------------------------------------
    // Helper: build a ManifestSchemaColumn
    // -----------------------------------------------------------------------
    fn col(name: &str, type_name: &str, nullable: bool, position: usize) -> ManifestSchemaColumn {
        ManifestSchemaColumn {
            name: name.to_string(),
            type_name: type_name.to_string(),
            nullable,
            position,
        }
    }

    fn manifest(columns: Vec<ManifestSchemaColumn>) -> ManifestSchema {
        ManifestSchema {
            column_count: columns.len(),
            columns,
        }
    }

    // -----------------------------------------------------------------------
    // parse_type_name – primitive scalar types
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_type_name_null() {
        assert_eq!(parse_type_name("Null"), DataType::Null);
    }

    #[test]
    fn test_parse_type_name_boolean() {
        assert_eq!(parse_type_name("Boolean"), DataType::Boolean);
    }

    #[test]
    fn test_parse_type_name_integer_types() {
        assert_eq!(parse_type_name("Int8"), DataType::Int8);
        assert_eq!(parse_type_name("Int16"), DataType::Int16);
        assert_eq!(parse_type_name("Int32"), DataType::Int32);
        assert_eq!(parse_type_name("Int64"), DataType::Int64);
    }

    #[test]
    fn test_parse_type_name_unsigned_integer_types() {
        assert_eq!(parse_type_name("UInt8"), DataType::UInt8);
        assert_eq!(parse_type_name("UInt16"), DataType::UInt16);
        assert_eq!(parse_type_name("UInt32"), DataType::UInt32);
        assert_eq!(parse_type_name("UInt64"), DataType::UInt64);
    }

    #[test]
    fn test_parse_type_name_float_types() {
        assert_eq!(parse_type_name("Float16"), DataType::Float16);
        assert_eq!(parse_type_name("Float32"), DataType::Float32);
        assert_eq!(parse_type_name("Float64"), DataType::Float64);
    }

    #[test]
    fn test_parse_type_name_string_types() {
        assert_eq!(parse_type_name("Utf8"), DataType::Utf8);
        assert_eq!(parse_type_name("LargeUtf8"), DataType::LargeUtf8);
        assert_eq!(parse_type_name("Utf8View"), DataType::Utf8View);
    }

    #[test]
    fn test_parse_type_name_binary_types() {
        assert_eq!(parse_type_name("Binary"), DataType::Binary);
        assert_eq!(parse_type_name("LargeBinary"), DataType::LargeBinary);
        assert_eq!(parse_type_name("BinaryView"), DataType::BinaryView);
    }

    #[test]
    fn test_parse_type_name_date_types() {
        assert_eq!(parse_type_name("Date32"), DataType::Date32);
        assert_eq!(parse_type_name("Date64"), DataType::Date64);
    }

    // -----------------------------------------------------------------------
    // parse_type_name – parameterised types
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_type_name_decimal128() {
        assert_eq!(
            parse_type_name("Decimal128(10, 2)"),
            DataType::Decimal128(10, 2)
        );
    }

    #[test]
    fn test_parse_type_name_decimal128_large_precision() {
        assert_eq!(
            parse_type_name("Decimal128(38, 18)"),
            DataType::Decimal128(38, 18)
        );
    }

    #[test]
    fn test_parse_type_name_decimal128_zero_scale() {
        assert_eq!(
            parse_type_name("Decimal128(10, 0)"),
            DataType::Decimal128(10, 0)
        );
    }

    #[test]
    fn test_parse_type_name_decimal256() {
        assert_eq!(
            parse_type_name("Decimal256(20, 5)"),
            DataType::Decimal256(20, 5)
        );
    }

    #[test]
    fn test_parse_type_name_timestamp_nanosecond_no_tz() {
        assert_eq!(
            parse_type_name("Timestamp(Nanosecond, None)"),
            DataType::Timestamp(TimeUnit::Nanosecond, None)
        );
    }

    #[test]
    fn test_parse_type_name_timestamp_millisecond_no_tz() {
        assert_eq!(
            parse_type_name("Timestamp(Millisecond, None)"),
            DataType::Timestamp(TimeUnit::Millisecond, None)
        );
    }

    #[test]
    fn test_parse_type_name_timestamp_microsecond_no_tz() {
        assert_eq!(
            parse_type_name("Timestamp(Microsecond, None)"),
            DataType::Timestamp(TimeUnit::Microsecond, None)
        );
    }

    #[test]
    fn test_parse_type_name_timestamp_second_no_tz() {
        assert_eq!(
            parse_type_name("Timestamp(Second, None)"),
            DataType::Timestamp(TimeUnit::Second, None)
        );
    }

    #[test]
    fn test_parse_type_name_timestamp_with_timezone() {
        assert_eq!(
            parse_type_name("Timestamp(Nanosecond, \"UTC\")"),
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
        );
    }

    #[test]
    fn test_parse_type_name_timestamp_with_offset_timezone() {
        assert_eq!(
            parse_type_name("Timestamp(Microsecond, \"+05:30\")"),
            DataType::Timestamp(TimeUnit::Microsecond, Some("+05:30".into()))
        );
    }

    #[test]
    fn test_parse_type_name_duration_all_units() {
        assert_eq!(
            parse_type_name("Duration(Second)"),
            DataType::Duration(TimeUnit::Second)
        );
        assert_eq!(
            parse_type_name("Duration(Millisecond)"),
            DataType::Duration(TimeUnit::Millisecond)
        );
        assert_eq!(
            parse_type_name("Duration(Microsecond)"),
            DataType::Duration(TimeUnit::Microsecond)
        );
        assert_eq!(
            parse_type_name("Duration(Nanosecond)"),
            DataType::Duration(TimeUnit::Nanosecond)
        );
    }

    #[test]
    fn test_parse_type_name_time32() {
        assert_eq!(
            parse_type_name("Time32(Second)"),
            DataType::Time32(TimeUnit::Second)
        );
        assert_eq!(
            parse_type_name("Time32(Millisecond)"),
            DataType::Time32(TimeUnit::Millisecond)
        );
    }

    #[test]
    fn test_parse_type_name_time64() {
        assert_eq!(
            parse_type_name("Time64(Microsecond)"),
            DataType::Time64(TimeUnit::Microsecond)
        );
        assert_eq!(
            parse_type_name("Time64(Nanosecond)"),
            DataType::Time64(TimeUnit::Nanosecond)
        );
    }

    #[test]
    fn test_parse_type_name_interval_all_units() {
        assert_eq!(
            parse_type_name("Interval(YearMonth)"),
            DataType::Interval(IntervalUnit::YearMonth)
        );
        assert_eq!(
            parse_type_name("Interval(DayTime)"),
            DataType::Interval(IntervalUnit::DayTime)
        );
        assert_eq!(
            parse_type_name("Interval(MonthDayNano)"),
            DataType::Interval(IntervalUnit::MonthDayNano)
        );
    }

    #[test]
    fn test_parse_type_name_fixed_size_binary() {
        assert_eq!(
            parse_type_name("FixedSizeBinary(16)"),
            DataType::FixedSizeBinary(16)
        );
    }

    #[test]
    fn test_parse_type_name_fixed_size_binary_large() {
        assert_eq!(
            parse_type_name("FixedSizeBinary(256)"),
            DataType::FixedSizeBinary(256)
        );
    }

    // -----------------------------------------------------------------------
    // parse_type_name – unknown / malformed input
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_type_name_unknown_falls_back_to_utf8() {
        assert_eq!(parse_type_name("UnknownType"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_empty_string_falls_back_to_utf8() {
        assert_eq!(parse_type_name(""), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_decimal_falls_back_to_utf8() {
        // Missing closing paren
        assert_eq!(parse_type_name("Decimal128(10, 2"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_decimal_no_scale() {
        assert_eq!(parse_type_name("Decimal128(10)"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_timestamp_bad_unit() {
        assert_eq!(
            parse_type_name("Timestamp(Picosecond, None)"),
            DataType::Utf8
        );
    }

    #[test]
    fn test_parse_type_name_malformed_timestamp_empty() {
        assert_eq!(parse_type_name("Timestamp()"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_duration_bad_unit() {
        assert_eq!(parse_type_name("Duration(Picosecond)"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_interval_bad_unit() {
        assert_eq!(parse_type_name("Interval(Weekly)"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_fixed_size_binary_no_size() {
        assert_eq!(parse_type_name("FixedSizeBinary()"), DataType::Utf8);
    }

    #[test]
    fn test_parse_type_name_malformed_fixed_size_binary_non_numeric() {
        assert_eq!(parse_type_name("FixedSizeBinary(abc)"), DataType::Utf8);
    }

    // -----------------------------------------------------------------------
    // parse_type_name – roundtrip via DataType::to_string()
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_type_name_roundtrip_scalars() {
        let types = vec![
            DataType::Null,
            DataType::Boolean,
            DataType::Int8,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::UInt8,
            DataType::UInt16,
            DataType::UInt32,
            DataType::UInt64,
            DataType::Float16,
            DataType::Float32,
            DataType::Float64,
            DataType::Utf8,
            DataType::LargeUtf8,
            DataType::Binary,
            DataType::LargeBinary,
            DataType::Date32,
            DataType::Date64,
        ];
        for dt in types {
            assert_eq!(
                parse_type_name(&dt.to_string()),
                dt,
                "roundtrip failed for {dt}"
            );
        }
    }

    #[test]
    fn test_parse_type_name_roundtrip_decimal128() {
        let dt = DataType::Decimal128(38, 10);
        assert_eq!(parse_type_name(&dt.to_string()), dt);
    }

    #[test]
    fn test_parse_type_name_roundtrip_decimal256() {
        let dt = DataType::Decimal256(76, 20);
        assert_eq!(parse_type_name(&dt.to_string()), dt);
    }

    #[test]
    fn test_parse_type_name_roundtrip_timestamp_no_tz() {
        // The server sends type names matching the format "Timestamp(Nanosecond, None)".
        // This is distinct from both Display (abbreviated units) and Debug (Some(...) wrapping).
        let expected = DataType::Timestamp(TimeUnit::Nanosecond, None);
        assert_eq!(parse_type_name("Timestamp(Nanosecond, None)"), expected);
    }

    #[test]
    fn test_parse_type_name_roundtrip_timestamp_with_tz() {
        let expected = DataType::Timestamp(TimeUnit::Microsecond, Some("America/New_York".into()));
        assert_eq!(
            parse_type_name("Timestamp(Microsecond, \"America/New_York\")"),
            expected
        );
    }

    #[test]
    fn test_parse_type_name_roundtrip_duration() {
        let expected = DataType::Duration(TimeUnit::Millisecond);
        assert_eq!(parse_type_name("Duration(Millisecond)"), expected);
    }

    #[test]
    fn test_parse_type_name_roundtrip_time32() {
        let expected = DataType::Time32(TimeUnit::Millisecond);
        assert_eq!(parse_type_name("Time32(Millisecond)"), expected);
    }

    #[test]
    fn test_parse_type_name_roundtrip_time64() {
        let expected = DataType::Time64(TimeUnit::Nanosecond);
        assert_eq!(parse_type_name("Time64(Nanosecond)"), expected);
    }

    #[test]
    fn test_parse_type_name_roundtrip_interval() {
        let dt = DataType::Interval(IntervalUnit::MonthDayNano);
        assert_eq!(parse_type_name(&dt.to_string()), dt);
    }

    #[test]
    fn test_parse_type_name_roundtrip_fixed_size_binary() {
        let dt = DataType::FixedSizeBinary(64);
        assert_eq!(parse_type_name(&dt.to_string()), dt);
    }

    // -----------------------------------------------------------------------
    // schema_from_manifest
    // -----------------------------------------------------------------------
    #[test]
    fn test_schema_from_manifest_empty_columns() {
        let m = manifest(vec![]);
        let schema = schema_from_manifest(&m);
        assert_eq!(schema.fields().len(), 0);
    }

    #[test]
    fn test_schema_from_manifest_single_column() {
        let m = manifest(vec![col("id", "Int64", false, 0)]);
        let schema = schema_from_manifest(&m);
        assert_eq!(schema.fields().len(), 1);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert!(!schema.field(0).is_nullable());
    }

    #[test]
    fn test_schema_from_manifest_multiple_columns() {
        let m = manifest(vec![
            col("id", "Int64", false, 0),
            col("name", "Utf8", true, 1),
            col("score", "Float64", true, 2),
            col("active", "Boolean", false, 3),
        ]);
        let schema = schema_from_manifest(&m);
        assert_eq!(schema.fields().len(), 4);

        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert!(!schema.field(0).is_nullable());

        assert_eq!(schema.field(1).name(), "name");
        assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
        assert!(schema.field(1).is_nullable());

        assert_eq!(schema.field(2).name(), "score");
        assert_eq!(schema.field(2).data_type(), &DataType::Float64);
        assert!(schema.field(2).is_nullable());

        assert_eq!(schema.field(3).name(), "active");
        assert_eq!(schema.field(3).data_type(), &DataType::Boolean);
        assert!(!schema.field(3).is_nullable());
    }

    #[test]
    fn test_schema_from_manifest_preserves_nullable() {
        let m = manifest(vec![
            col("a", "Int32", true, 0),
            col("b", "Int32", false, 1),
        ]);
        let schema = schema_from_manifest(&m);
        assert!(schema.field(0).is_nullable());
        assert!(!schema.field(1).is_nullable());
    }

    #[test]
    fn test_schema_from_manifest_complex_types() {
        let m = manifest(vec![
            col("ts", "Timestamp(Nanosecond, None)", true, 0),
            col("price", "Decimal128(18, 4)", true, 1),
            col("data", "FixedSizeBinary(32)", false, 2),
        ]);
        let schema = schema_from_manifest(&m);
        assert_eq!(schema.fields().len(), 3);

        assert_eq!(
            schema.field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Nanosecond, None)
        );
        assert_eq!(schema.field(1).data_type(), &DataType::Decimal128(18, 4));
        assert_eq!(schema.field(2).data_type(), &DataType::FixedSizeBinary(32));
    }

    #[test]
    fn test_schema_from_manifest_unknown_type_becomes_utf8() {
        let m = manifest(vec![col("mystery", "SomeNewType", true, 0)]);
        let schema = schema_from_manifest(&m);
        assert_eq!(schema.field(0).data_type(), &DataType::Utf8);
    }

    #[test]
    fn test_schema_from_manifest_timestamp_with_tz() {
        let m = manifest(vec![col(
            "event_time",
            "Timestamp(Microsecond, \"UTC\")",
            true,
            0,
        )]);
        let schema = schema_from_manifest(&m);
        assert_eq!(
            schema.field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
    }

    // -----------------------------------------------------------------------
    // ManifestMetadata JSON deserialization
    // -----------------------------------------------------------------------
    #[test]
    fn test_manifest_metadata_deserialize_with_schema() {
        let json = serde_json::json!({
            "format": "ARROW_IPC",
            "schema": {
                "column_count": 2,
                "columns": [
                    {"name": "id", "type_name": "Int64", "nullable": false, "position": 0},
                    {"name": "name", "type_name": "Utf8", "nullable": true, "position": 1}
                ]
            },
            "total_row_count": 0,
            "total_chunk_count": 0
        });
        let meta: ManifestMetadata =
            serde_json::from_value(json).expect("should deserialize ManifestMetadata");
        assert_eq!(meta.total_row_count, 0);
        assert_eq!(meta.total_chunk_count, 0);

        let schema_info = meta.schema.expect("schema should be present");
        assert_eq!(schema_info.column_count, 2);
        assert_eq!(schema_info.columns.len(), 2);
        assert_eq!(schema_info.columns[0].name, "id");
        assert_eq!(schema_info.columns[0].type_name, "Int64");
        assert!(!schema_info.columns[0].nullable);
        assert_eq!(schema_info.columns[1].name, "name");
        assert_eq!(schema_info.columns[1].type_name, "Utf8");
        assert!(schema_info.columns[1].nullable);
    }

    #[test]
    fn test_manifest_metadata_deserialize_without_schema() {
        let json = serde_json::json!({
            "total_row_count": 100,
            "total_chunk_count": 2
        });
        let meta: ManifestMetadata =
            serde_json::from_value(json).expect("should deserialize ManifestMetadata");
        assert_eq!(meta.total_row_count, 100);
        assert_eq!(meta.total_chunk_count, 2);
        assert!(meta.schema.is_none());
        assert!(meta.format.is_none());
    }

    #[test]
    fn test_manifest_metadata_deserialize_full_response() {
        // Simulate the full JSON payload the server returns for a 0-row query.
        let json = serde_json::json!({
            "format": "ARROW_IPC",
            "schema": {
                "column_count": 3,
                "columns": [
                    {"name": "customer_id", "type_name": "Int32", "nullable": false, "position": 0},
                    {"name": "total_sales", "type_name": "Decimal128(18, 2)", "nullable": true, "position": 1},
                    {"name": "last_order", "type_name": "Timestamp(Millisecond, None)", "nullable": true, "position": 2}
                ]
            },
            "total_row_count": 0,
            "total_chunk_count": 0
        });
        let meta: ManifestMetadata =
            serde_json::from_value(json).expect("should deserialize ManifestMetadata");
        let schema = schema_from_manifest(meta.schema.as_ref().expect("schema should be present"));

        assert_eq!(schema.fields().len(), 3);
        assert_eq!(schema.field(0).name(), "customer_id");
        assert_eq!(schema.field(0).data_type(), &DataType::Int32);
        assert!(!schema.field(0).is_nullable());

        assert_eq!(schema.field(1).name(), "total_sales");
        assert_eq!(schema.field(1).data_type(), &DataType::Decimal128(18, 2));
        assert!(schema.field(1).is_nullable());

        assert_eq!(schema.field(2).name(), "last_order");
        assert_eq!(
            schema.field(2).data_type(),
            &DataType::Timestamp(TimeUnit::Millisecond, None)
        );
        assert!(schema.field(2).is_nullable());
    }

    // -----------------------------------------------------------------------
    // QueryResultStream::empty_with_schema
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_empty_with_schema_yields_one_empty_batch() {
        let client = test_http_client("http://unused:9999");
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Utf8, true),
        ]));
        let mut stream = QueryResultStream::empty_with_schema(
            client,
            "test-query-id".to_string(),
            Arc::clone(&schema),
        );

        // First poll: should yield an empty batch with the correct schema.
        let item = stream.next().await;
        assert!(item.is_some(), "stream should yield one item");
        let batch = item.expect("should have item").expect("should be Ok");
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.schema(), schema);
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.schema().field(0).name(), "a");
        assert_eq!(batch.schema().field(1).name(), "b");

        // Second poll: stream should be done.
        let item = stream.next().await;
        assert!(item.is_none(), "stream should be exhausted");
    }

    #[tokio::test]
    async fn test_empty_with_schema_complex_types() {
        let client = test_http_client("http://unused:9999");
        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "ts",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                true,
            ),
            Field::new("amount", DataType::Decimal128(18, 4), false),
            Field::new("payload", DataType::FixedSizeBinary(64), true),
        ]));
        let mut stream = QueryResultStream::empty_with_schema(
            client,
            "q-complex".to_string(),
            Arc::clone(&schema),
        );

        let batch = stream
            .next()
            .await
            .expect("should yield one item")
            .expect("should be Ok");
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.schema(), schema);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_empty_with_schema_no_columns() {
        let client = test_http_client("http://unused:9999");
        let schema = Arc::new(Schema::empty());
        let mut stream = QueryResultStream::empty_with_schema(
            client,
            "q-empty-schema".to_string(),
            Arc::clone(&schema),
        );

        let batch = stream
            .next()
            .await
            .expect("should yield one item")
            .expect("should be Ok");
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.num_columns(), 0);
        assert!(stream.next().await.is_none());
    }

    // -----------------------------------------------------------------------
    // End-to-end: manifest JSON → schema → empty RecordBatch
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_manifest_to_empty_batch_end_to_end() {
        // Simulates the full path taken by results_stream when total_rows == 0:
        // 1. Deserialize the manifest JSON
        // 2. Build the Arrow schema via schema_from_manifest
        // 3. Create a QueryResultStream::empty_with_schema
        // 4. Verify the yielded batch has the correct schema and 0 rows
        let json = serde_json::json!({
            "format": "ARROW_IPC",
            "schema": {
                "column_count": 4,
                "columns": [
                    {"name": "order_id", "type_name": "Int64", "nullable": false, "position": 0},
                    {"name": "customer", "type_name": "Utf8", "nullable": true, "position": 1},
                    {"name": "amount", "type_name": "Decimal128(10, 2)", "nullable": true, "position": 2},
                    {"name": "created_at", "type_name": "Timestamp(Microsecond, \"UTC\")", "nullable": false, "position": 3}
                ]
            },
            "total_row_count": 0,
            "total_chunk_count": 0
        });
        let meta: ManifestMetadata = serde_json::from_value(json).expect("should deserialize");
        let schema = schema_from_manifest(meta.schema.as_ref().expect("schema present"));

        let client = test_http_client("http://unused:9999");
        let mut stream =
            QueryResultStream::empty_with_schema(client, "e2e-query".to_string(), schema);

        let batch = stream
            .next()
            .await
            .expect("should yield one item")
            .expect("should be Ok");

        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.num_columns(), 4);
        assert_eq!(batch.schema().field(0).name(), "order_id");
        assert_eq!(batch.schema().field(0).data_type(), &DataType::Int64);
        assert!(!batch.schema().field(0).is_nullable());

        assert_eq!(batch.schema().field(1).name(), "customer");
        assert_eq!(batch.schema().field(1).data_type(), &DataType::Utf8);
        assert!(batch.schema().field(1).is_nullable());

        assert_eq!(batch.schema().field(2).name(), "amount");
        assert_eq!(
            batch.schema().field(2).data_type(),
            &DataType::Decimal128(10, 2)
        );

        assert_eq!(batch.schema().field(3).name(), "created_at");
        assert_eq!(
            batch.schema().field(3).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        assert!(!batch.schema().field(3).is_nullable());

        // Stream exhausted
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_manifest_to_empty_batch_all_nullable() {
        let json = serde_json::json!({
            "format": "ARROW_IPC",
            "schema": {
                "column_count": 2,
                "columns": [
                    {"name": "x", "type_name": "Float32", "nullable": true, "position": 0},
                    {"name": "y", "type_name": "Float32", "nullable": true, "position": 1}
                ]
            },
            "total_row_count": 0,
            "total_chunk_count": 0
        });
        let meta: ManifestMetadata = serde_json::from_value(json).expect("should deserialize");
        let schema = schema_from_manifest(meta.schema.as_ref().expect("schema present"));

        let client = test_http_client("http://unused:9999");
        let mut stream =
            QueryResultStream::empty_with_schema(client, "all-nullable".to_string(), schema);

        let batch = stream
            .next()
            .await
            .expect("should yield")
            .expect("should be Ok");
        assert_eq!(batch.num_rows(), 0);
        assert!(batch.schema().field(0).is_nullable());
        assert!(batch.schema().field(1).is_nullable());
        assert!(stream.next().await.is_none());
    }

    // -----------------------------------------------------------------------
    // QueryStatus helpers
    // -----------------------------------------------------------------------
    #[test]
    fn test_query_status_is_success() {
        assert!(QueryStatus::Succeeded.is_success());
        assert!(!QueryStatus::Failed.is_success());
        assert!(!QueryStatus::Pending.is_success());
        assert!(!QueryStatus::Running.is_success());
        assert!(!QueryStatus::Cancelled.is_success());
        assert!(!QueryStatus::Closed.is_success());
    }

    #[test]
    fn test_query_status_is_terminal() {
        assert!(QueryStatus::Succeeded.is_terminal());
        assert!(QueryStatus::Failed.is_terminal());
        assert!(QueryStatus::Cancelled.is_terminal());
        assert!(QueryStatus::Closed.is_terminal());
        assert!(!QueryStatus::Pending.is_terminal());
        assert!(!QueryStatus::Running.is_terminal());
    }

    #[test]
    fn test_query_status_is_running() {
        assert!(QueryStatus::Pending.is_running());
        assert!(QueryStatus::Running.is_running());
        assert!(!QueryStatus::Succeeded.is_running());
        assert!(!QueryStatus::Failed.is_running());
    }

    #[test]
    fn test_query_status_display() {
        assert_eq!(QueryStatus::Pending.to_string(), "PENDING");
        assert_eq!(QueryStatus::Running.to_string(), "RUNNING");
        assert_eq!(QueryStatus::Succeeded.to_string(), "SUCCEEDED");
        assert_eq!(QueryStatus::Failed.to_string(), "FAILED");
        assert_eq!(QueryStatus::Cancelled.to_string(), "CANCELLED");
        assert_eq!(QueryStatus::Closed.to_string(), "CLOSED");
    }

    #[test]
    fn test_query_status_serde_roundtrip() {
        let statuses = vec![
            QueryStatus::Pending,
            QueryStatus::Running,
            QueryStatus::Succeeded,
            QueryStatus::Failed,
            QueryStatus::Cancelled,
            QueryStatus::Closed,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).expect("should serialize");
            let back: QueryStatus = serde_json::from_str(&json).expect("should deserialize");
            assert_eq!(back, status, "roundtrip failed for {status}");
        }
    }
}
