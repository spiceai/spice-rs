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
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

/// Default poll interval for checking query status.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Errors that can occur during async query operations.
#[derive(Debug, Snafu)]
pub enum QueryError {
    /// Failed to submit the query.
    #[snafu(display("Failed to submit query: {message}"))]
    SubmitFailed { message: String },

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

    /// HTTP request failed.
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
            _ => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                Err(QueryError::SubmitFailed {
                    message: format!("HTTP {status}: {text}"),
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
            _ => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                Err(QueryError::HttpError {
                    message: format!("HTTP {status}: {text}"),
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
            _ => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                Err(QueryError::HttpError {
                    message: format!("HTTP {status}: {text}"),
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
            _ => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                Err(QueryError::HttpError {
                    message: format!("HTTP {status}: {text}"),
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
            _ => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                Err(QueryError::HttpError {
                    message: format!("HTTP {status}: {text}"),
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
            409 => Err(QueryError::HttpError {
                message: format!("Query {query_id} has already completed"),
            }),
            _ => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                Err(QueryError::HttpError {
                    message: format!("HTTP {status}: {text}"),
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
}

#[derive(Debug, Deserialize)]
pub(crate) struct SubmitResponse {
    pub query_id: String,
    #[allow(dead_code)]
    pub status: StatusResponse,
    #[allow(dead_code)]
    pub status_url: String,
    #[allow(dead_code)]
    pub results_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StatusResponse {
    pub state: String,
    #[serde(default)]
    pub error: Option<ErrorResponse>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ErrorResponse {
    #[serde(default)]
    pub error_code: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryInfoResponse {
    pub query_id: String,
    pub status: StatusResponse,
    #[serde(default)]
    pub result: Option<ResultMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ResultMetadata {
    pub total_row_count: u64,
    pub total_chunk_count: u64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ResultChunkResponse {
    pub query_id: String,
    pub chunk_index: usize,
    pub row_count: usize,
    pub data: Vec<serde_json::Value>,
}

impl StatusResponse {
    pub fn to_status(&self) -> QueryStatus {
        match self.state.to_uppercase().as_str() {
            "RUNNING" => QueryStatus::Running,
            "SUCCEEDED" => QueryStatus::Succeeded,
            "FAILED" => QueryStatus::Failed,
            "CANCELLED" => QueryStatus::Cancelled,
            "CLOSED" => QueryStatus::Closed,
            // Default to Pending for "PENDING" and unknown states
            _ => QueryStatus::Pending,
        }
    }
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
        Ok(response.to_status())
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
            status: response.status.to_status(),
            error: response.status.error.map(|e| QueryErrorInfo {
                error_code: e.error_code,
                message: e.message,
            }),
            result: response.result.map(|r| QueryResult {
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
        let info = self.info().await?;

        if !info.status.is_success() {
            return Err(QueryError::NotReady {
                query_id: self.query_id.clone(),
            });
        }

        let total_chunks = info.result.map_or(1, |r| r.total_chunks);

        let mut all_batches = Vec::new();
        #[allow(clippy::cast_possible_truncation)]
        for chunk_index in 0..total_chunks {
            let batches = self
                .client
                .get_results_arrow(&self.query_id, chunk_index as usize)
                .await?;
            all_batches.extend(batches);
        }

        Ok(all_batches)
    }

    /// Retrieves a specific result chunk as Arrow record batches.
    ///
    /// # Errors
    ///
    /// Returns an error if the chunk is not found or the query is not complete.
    pub async fn results_chunk(&self, chunk_index: usize) -> Result<Vec<RecordBatch>, QueryError> {
        self.client
            .get_results_arrow(&self.query_id, chunk_index)
            .await
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
            status: response.status.to_status(),
            error: response.status.error.map(|e| QueryErrorInfo {
                error_code: e.error_code,
                message: e.message,
            }),
            result: response.result.map(|r| QueryResult {
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
