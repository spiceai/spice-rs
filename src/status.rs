//! Runtime health and per-component status.
//!
//! Two levels of detail are available. [`SpiceClient::is_ready`] is a single boolean
//! for the whole runtime, backed by `GET /v1/ready`. [`SpiceClient::runtime_status`]
//! reports the state of each connection individually, backed by `GET /v1/status`, and
//! so can distinguish a runtime that is still initializing from one whose Flight
//! endpoint is failing.

use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::fmt;

use crate::query::QueryHttpClient;

/// Errors returned when querying runtime health or status.
#[derive(Debug, Snafu)]
pub enum StatusError {
    /// The HTTP endpoint was not configured on the client.
    #[snafu(display("HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."))]
    HttpNotConfigured,

    /// HTTP request failed with an error response.
    ///
    /// Raised by both [`SpiceClient::runtime_status`] and [`SpiceClient::is_ready`],
    /// so the message names the endpoint that failed rather than assuming either one.
    #[snafu(display("Failed to query {url} (HTTP {status_code}): {response_body}"))]
    RequestFailed {
        /// Endpoint that returned the error response.
        url: String,
        /// HTTP status code returned by the server.
        status_code: u16,
        /// Response body from the server.
        response_body: String,
    },

    /// HTTP transport error.
    ///
    /// Raised by both [`SpiceClient::runtime_status`] and [`SpiceClient::is_ready`].
    #[snafu(display("Failed to query {url}: {message}"))]
    HttpError {
        /// Endpoint that could not be reached.
        url: String,
        /// Description of the transport failure.
        message: String,
    },

    /// Failed to parse the server response.
    #[snafu(display("Failed to parse runtime status response: {message}"))]
    ParseError {
        /// Description of the parse failure.
        message: String,
    },
}

/// The state of a single runtime component.
///
/// Mirrors the runtime's `ComponentStatus`. Unknown variants are preserved in
/// [`ComponentStatus::Other`] so that a newer runtime does not break deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComponentStatus {
    /// The component is initializing and not yet ready.
    Initializing,
    /// The component is ready to accept connections.
    Ready,
    /// The component is disabled and not running.
    Disabled,
    /// An error occurred in the component.
    Error,
    /// The component is refreshing its state.
    Refreshing,
    /// The component is shutting down.
    ShuttingDown,
    /// The component is configured but not loaded yet.
    NotLoaded,
    /// A status this version of the SDK does not know about.
    #[serde(untagged)]
    Other(String),
}

impl ComponentStatus {
    /// Returns `true` if the component is ready to accept connections.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, ComponentStatus::Ready)
    }

    /// Returns `true` if the component is in an error state.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, ComponentStatus::Error)
    }
}

impl fmt::Display for ComponentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ComponentStatus::Initializing => write!(f, "Initializing"),
            ComponentStatus::Ready => write!(f, "Ready"),
            ComponentStatus::Disabled => write!(f, "Disabled"),
            ComponentStatus::Error => write!(f, "Error"),
            ComponentStatus::Refreshing => write!(f, "Refreshing"),
            ComponentStatus::ShuttingDown => write!(f, "ShuttingDown"),
            ComponentStatus::NotLoaded => write!(f, "NotLoaded"),
            ComponentStatus::Other(status) => write!(f, "{status}"),
        }
    }
}

/// The status of one runtime connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionDetails {
    /// Name of the connection: `http`, `flight`, `metrics` or `opentelemetry`.
    pub name: String,
    /// Endpoint the connection is served on, or `N/A` when the component is disabled.
    pub endpoint: String,
    /// Status of the component.
    pub status: ComponentStatus,
}

impl ConnectionDetails {
    /// Returns `true` if this component is ready to accept connections.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status.is_ready()
    }
}

impl QueryHttpClient {
    /// Fetches per-component runtime status from `GET /v1/status`.
    pub(crate) async fn runtime_status(&self) -> Result<Vec<ConnectionDetails>, StatusError> {
        let url = format!("{}/v1/status", self.base_url());

        let response = self
            .authorized(self.client().get(&url))
            .send()
            .await
            .map_err(|e| StatusError::HttpError {
                url: url.clone(),
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => response.json().await.map_err(|e| StatusError::ParseError {
                message: e.to_string(),
            }),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(StatusError::RequestFailed {
                    url,
                    status_code,
                    response_body,
                })
            }
        }
    }

    /// Probes `GET /v1/ready`. `200` means ready, `503` means not ready.
    pub(crate) async fn is_ready(&self) -> Result<bool, StatusError> {
        let url = format!("{}/v1/ready", self.base_url());

        let response = self
            .authorized(self.client().get(&url))
            .send()
            .await
            .map_err(|e| StatusError::HttpError {
                url: url.clone(),
                message: e.to_string(),
            })?;

        match response.status().as_u16() {
            200 => Ok(true),
            503 => Ok(false),
            status_code => {
                let response_body = response.text().await.unwrap_or_default();
                Err(StatusError::RequestFailed {
                    url,
                    status_code,
                    response_body,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_statuses_round_trip() {
        for (json, expected) in [
            ("\"Initializing\"", ComponentStatus::Initializing),
            ("\"Ready\"", ComponentStatus::Ready),
            ("\"Disabled\"", ComponentStatus::Disabled),
            ("\"Error\"", ComponentStatus::Error),
            ("\"Refreshing\"", ComponentStatus::Refreshing),
            ("\"ShuttingDown\"", ComponentStatus::ShuttingDown),
            ("\"NotLoaded\"", ComponentStatus::NotLoaded),
        ] {
            let parsed: ComponentStatus =
                serde_json::from_str(json).expect("deserialize component status");
            assert_eq!(parsed, expected);
            assert_eq!(
                serde_json::to_string(&parsed).expect("serialize component status"),
                json
            );
        }
    }

    #[test]
    fn unknown_status_is_preserved() {
        let parsed: ComponentStatus =
            serde_json::from_str("\"SomethingNew\"").expect("deserialize unknown status");
        assert_eq!(parsed, ComponentStatus::Other("SomethingNew".to_string()));
        assert_eq!(parsed.to_string(), "SomethingNew");
        assert!(!parsed.is_ready());
    }

    #[test]
    fn status_predicates() {
        assert!(ComponentStatus::Ready.is_ready());
        assert!(!ComponentStatus::Initializing.is_ready());
        assert!(ComponentStatus::Error.is_error());
        assert!(!ComponentStatus::Ready.is_error());
    }

    #[test]
    fn connection_details_deserialize() {
        let body = r#"[
            {"name":"http","endpoint":"127.0.0.1:8090","status":"Ready"},
            {"name":"flight","endpoint":"127.0.0.1:50051","status":"Initializing"},
            {"name":"metrics","endpoint":"N/A","status":"Disabled"}
        ]"#;

        let details: Vec<ConnectionDetails> =
            serde_json::from_str(body).expect("deserialize connection details");

        assert_eq!(details.len(), 3);
        assert_eq!(details[0].name, "http");
        assert!(details[0].is_ready());
        assert_eq!(details[1].status, ComponentStatus::Initializing);
        assert!(!details[1].is_ready());
        assert_eq!(details[2].endpoint, "N/A");
    }

    #[test]
    fn request_errors_name_the_endpoint_that_failed() {
        // `RequestFailed`/`HttpError` are shared by `runtime_status` (/v1/status) and
        // `is_ready` (/v1/ready), so the message has to say which one was being queried.
        let request_failed = StatusError::RequestFailed {
            url: "http://localhost:8090/v1/ready".to_string(),
            status_code: 401,
            response_body: "unauthorized".to_string(),
        };
        assert_eq!(
            request_failed.to_string(),
            "Failed to query http://localhost:8090/v1/ready (HTTP 401): unauthorized"
        );

        let http_error = StatusError::HttpError {
            url: "http://localhost:8090/v1/status".to_string(),
            message: "connection refused".to_string(),
        };
        assert_eq!(
            http_error.to_string(),
            "Failed to query http://localhost:8090/v1/status: connection refused"
        );
    }
}
