//! Listing and cancelling *synchronous* queries running on the runtime.
//!
//! This is distinct from the async query jobs in [`crate::query`]. A synchronous
//! query is one started by [`Client::sql()`](crate::Client::sql), FlightSQL,
//! `/v1/sql`, NSQL, or `/v1/search` — it streams results back on the connection
//! that started it. Async jobs are submitted with
//! [`Client::query()`](crate::Client::query), are polled for completion, and
//! require the runtime to be running in cluster mode.
//!
//! The runtime assigns a `query_id` to every synchronous query but does not
//! return it to the client that submitted it, so the two operations here are
//! used together: list the active queries to find the one you want, then cancel
//! it by id. Listing and cancellation are both scoped to the caller, so a client
//! only ever sees and cancels its own queries.
//!
//! # Example
//!
//! ```no_run
//! use spiceai::ClientBuilder;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     let client = ClientBuilder::new()
//!         .http_url("http://localhost:8090")
//!         .build()
//!         .await?;
//!
//!     let active = client.active_queries().await?;
//!     for query in &active.queries {
//!         println!("{} [{}] {}", query.query_id, query.protocol, query.sql_preview);
//!     }
//!
//!     if let Some(query) = active.queries.first() {
//!         client.cancel_active_query(&query.query_id).await?;
//!     }
//!
//!     Ok(())
//! }
//! ```

use serde::Deserialize;
use snafu::Snafu;

/// Errors that can occur while listing or cancelling active synchronous queries.
#[derive(Debug, Snafu)]
pub enum ActiveQueryError {
    /// No active synchronous query with this id belongs to the caller.
    ///
    /// The runtime reports a query submitted by another caller the same way it
    /// reports one that does not exist, so a client cannot probe for other
    /// callers' query ids.
    #[snafu(display(
        "No active query '{query_id}' found. It may have already finished, or it was submitted by a different client."
    ))]
    NotFound { query_id: String },

    /// The supplied id is not a UUID, so it cannot name a query.
    #[snafu(display(
        "Query id '{query_id}' is not a valid UUID. Use the query_id from active_queries()."
    ))]
    InvalidQueryId { query_id: String },

    /// The configured API key does not grant write access.
    #[snafu(display(
        "The configured API key does not allow cancelling queries. Use a key with write access."
    ))]
    WriteAccessRequired,

    /// The request failed with an unexpected status code.
    #[snafu(display("Request failed (HTTP {status_code}): {response_body}"))]
    RequestFailed {
        /// HTTP status code returned by the runtime.
        status_code: u16,
        /// Response body returned by the runtime.
        response_body: String,
    },

    /// HTTP transport error.
    #[snafu(display("Request failed: {message}"))]
    HttpError { message: String },

    /// The response could not be parsed.
    #[snafu(display("Failed to parse response: {message}"))]
    ParseError { message: String },
}

/// A synchronous query currently executing on the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ActiveQuery {
    /// Server-assigned id, used to cancel the query.
    pub query_id: String,
    /// The protocol the query arrived on, such as `flight` or `http`.
    pub protocol: String,
    /// The query's SQL, truncated by the runtime for display.
    pub sql_preview: String,
    /// When the query started, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
}

/// The set of synchronous queries the caller currently has running.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ActiveQueryList {
    /// The active queries, most recently started first.
    pub queries: Vec<ActiveQuery>,
    /// Number of active queries reported by the runtime.
    pub total_count: usize,
}

/// The runtime's response to a successful cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CancelActiveQueryResponse {
    /// The id of the query that was cancelled.
    pub query_id: String,
    /// The query's state after cancellation, such as `cancelled`.
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_an_active_query_list() {
        let body = r#"{
            "queries": [
                {
                    "query_id": "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70",
                    "protocol": "flight",
                    "sql_preview": "SELECT * FROM taxi_trips",
                    "started_at_ms": 1750000000000
                }
            ],
            "total_count": 1
        }"#;

        let list: ActiveQueryList = serde_json::from_str(body).expect("deserialize list");
        assert_eq!(list.total_count, 1);
        assert_eq!(list.queries.len(), 1);
        assert_eq!(list.queries[0].protocol, "flight");
        assert_eq!(list.queries[0].sql_preview, "SELECT * FROM taxi_trips");
        assert_eq!(list.queries[0].started_at_ms, 1_750_000_000_000);
    }

    #[test]
    fn deserializes_an_empty_active_query_list() {
        let list: ActiveQueryList =
            serde_json::from_str(r#"{"queries":[],"total_count":0}"#).expect("deserialize list");
        assert_eq!(list.total_count, 0);
        assert!(list.queries.is_empty());
    }

    #[test]
    fn deserializes_a_cancel_response() {
        let response: CancelActiveQueryResponse = serde_json::from_str(
            r#"{"query_id":"0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70","status":"cancelled"}"#,
        )
        .expect("deserialize cancel response");
        assert_eq!(response.status, "cancelled");
    }

    #[test]
    fn not_found_error_explains_both_causes() {
        let message = ActiveQueryError::NotFound {
            query_id: "abc".to_string(),
        }
        .to_string();
        assert!(message.contains("abc"));
        assert!(message.contains("already finished"));
    }

    #[test]
    fn invalid_query_id_error_points_at_active_queries() {
        let message = ActiveQueryError::InvalidQueryId {
            query_id: "not-a-uuid".to_string(),
        }
        .to_string();
        assert!(message.contains("not-a-uuid"));
        assert!(message.contains("active_queries()"));
    }
}
