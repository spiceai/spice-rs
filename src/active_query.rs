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
//! it by id.
//!
//! # Scope
//!
//! Two boundaries apply, and a query is reachable only inside both.
//!
//! **One runtime instance.** The runtime tracks active synchronous queries in
//! memory, per instance, and these endpoints report only what the instance
//! answering them knows. This client configures its Flight and HTTP endpoints
//! independently, so behind a load balancer the query submitted over Flight may
//! be running on a different instance than the one answering here — it will not
//! be listed, and its id reports as not found. Point
//! [`http_url()`](crate::ClientBuilder::http_url) at the instance running the
//! query.
//!
//! **One authenticated principal**, not a [`Client`](crate::Client) instance.
//! The principal is whatever credential the runtime authenticates — an API key
//! or a client certificate — so every client presenting the same credential
//! shares one scope and can list and cancel the others' queries. Only requests
//! for which the runtime establishes no principal at all share the `public`
//! scope. A query outside the caller's scope is reported as if it did not
//! exist.
//!
//! **Runtime version.** Principal scoping on these two endpoints landed in
//! [spiceai/spiceai#12841](https://github.com/spiceai/spiceai/pull/12841) and is
//! in no runtime release up to and including `v2.1.5`. Against an earlier
//! runtime both calls operate on every active query the instance holds, for any
//! caller with write access.
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
    /// No active synchronous query with this id is in the caller's scope.
    ///
    /// The runtime reports a query submitted under another principal the same
    /// way it reports one that does not exist, so a caller cannot probe for
    /// other principals' query ids.
    #[snafu(display(
        "No active query '{query_id}' found. It may have already finished, it was submitted under a different principal, or it is running on another runtime instance."
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
    /// The protocol the query arrived on: `http`, `flight`, `flightsql`, or
    /// `internal`.
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

/// Whether `query_id` has the shape the runtime parses as a UUID.
///
/// The ids this SDK cancels always come from [`ActiveQuery::query_id`], so a
/// value that is not a UUID cannot name a running query. Checking locally keeps
/// such a value out of the request path entirely: `.` and `..` are unreserved,
/// so percent-encoding leaves them intact and the URL parser then resolves them
/// away — `..` turns `/v1/sql/{id}/cancel` into a POST at a route the caller
/// never asked for.
pub(crate) fn is_uuid(query_id: &str) -> bool {
    let bytes = query_id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            *byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
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
    fn is_uuid_accepts_the_ids_the_runtime_hands_out() {
        assert!(is_uuid("0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70"));
        // The runtime parses either case.
        assert!(is_uuid("0198F0A1-9C3D-7C4E-8A11-2B3C4D5E6F70"));
    }

    #[test]
    fn is_uuid_rejects_anything_that_could_reroute_a_request() {
        for id in [
            "",
            ".",
            "..",
            "../queries/escape",
            "not-a-uuid",
            // Right length, wrong shape: hyphens off their positions.
            "0198f0a19c3d-7c4e-8a11-2b3c4d5e6f70-",
            // Right shape, a non-hex digit.
            "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f7g",
            // A trailing segment appended to a valid id.
            "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70/cancel",
        ] {
            assert!(!is_uuid(id), "{id:?} should not be accepted as a UUID");
        }
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
