use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use snafu::Snafu;

/// Errors returned when searching.
#[derive(Debug, Snafu)]
pub enum SearchError {
    /// The search request was rejected before being sent.
    #[snafu(display("Invalid search request: {message}"))]
    InvalidRequest { message: String },

    /// The runtime rejected the search.
    #[snafu(display("Search failed (HTTP {status_code}): {response_body}"))]
    SearchFailed {
        /// HTTP status code returned by the server.
        status_code: u16,
        /// Response body from the server.
        response_body: String,
    },

    /// HTTP transport error.
    #[snafu(display("Search failed: {message}"))]
    HttpError { message: String },

    /// Failed to parse the server response.
    #[snafu(display("Failed to parse search response: {message}"))]
    ParseError { message: String },
}

/// A search against the runtime's `/v1/search` endpoint.
///
/// Only the search text is required. Adding keywords turns the search into a
/// hybrid one: the runtime runs a lexical pass alongside the vector pass and
/// combines the scores into a single ranking.
///
/// ```
/// use spiceai::SearchRequest;
///
/// let request = SearchRequest::new("tokyo plane tickets")
///     .with_datasets(["app_messages"])
///     .with_limit(3)
///     .with_additional_columns(["timestamp"]);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SearchRequest {
    /// The text to find similar documents for.
    pub text: String,

    /// Datasets to search. When empty, the runtime searches every searchable
    /// dataset.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub datasets: Vec<String>,

    /// Maximum matches to return per dataset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// A SQL predicate filtering candidate rows, without the leading `WHERE` —
    /// for example `"user_id = 42"`.
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub where_cond: Option<String>,

    /// Extra columns to return with each match. A primary key column is
    /// returned in [`SearchMatch::primary_key`], the rest in
    /// [`SearchMatch::data`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_columns: Vec<String>,

    /// Keywords driving the lexical pass of a hybrid search.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

impl SearchRequest {
    /// Creates a search for documents similar to `text`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    /// Restricts the search to the named datasets.
    #[must_use]
    pub fn with_datasets<I, S>(mut self, datasets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.datasets = datasets.into_iter().map(Into::into).collect();
        self
    }

    /// Caps the number of matches returned per dataset.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Filters candidate rows with a SQL predicate, without the leading `WHERE`.
    #[must_use]
    pub fn with_where(mut self, where_cond: impl Into<String>) -> Self {
        self.where_cond = Some(where_cond.into());
        self
    }

    /// Returns extra columns alongside each match.
    #[must_use]
    pub fn with_additional_columns<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.additional_columns = columns.into_iter().map(Into::into).collect();
        self
    }

    /// Adds a lexical pass to the search, producing a hybrid ranking.
    #[must_use]
    pub fn with_keywords<I, S>(mut self, keywords: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.keywords = keywords.into_iter().map(Into::into).collect();
        self
    }

    /// Rejects requests the runtime would answer with a `400`, so the error
    /// names the field to fix rather than reporting a status code.
    pub(crate) fn validate(&self) -> Result<(), SearchError> {
        if self.text.trim().is_empty() {
            return Err(SearchError::InvalidRequest {
                message: "text is required and must be a non-empty search string".to_string(),
            });
        }
        if self.limit == Some(0) {
            return Err(SearchError::InvalidRequest {
                message: "limit must be greater than 0".to_string(),
            });
        }
        Ok(())
    }
}

/// A single document matched by a search.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SearchMatch {
    /// The dataset the match was found in.
    #[serde(default)]
    pub dataset: String,

    /// The match's similarity to the query. Higher is more similar.
    #[serde(rename = "_score", default)]
    pub score: f64,

    /// The matched values, keyed by the column they came from. Each value is a
    /// list because one column can contribute several chunks to a single match.
    #[serde(default)]
    pub matches: HashMap<String, Vec<serde_json::Value>>,

    /// The primary key columns identifying the matched row. Empty when the
    /// dataset declares no primary key.
    #[serde(default)]
    pub primary_key: HashMap<String, serde_json::Value>,

    /// Any additional columns that were requested.
    #[serde(default)]
    pub data: HashMap<String, serde_json::Value>,

    /// Extra per-match metadata the runtime attached.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// The result of a single search.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SearchResponse {
    /// The matches, ordered by descending score.
    #[serde(default)]
    pub results: Vec<SearchMatch>,

    /// How long the runtime reported the search took, in milliseconds.
    #[serde(default)]
    pub duration_ms: u128,
}

impl SearchResponse {
    /// Returns the number of matches.
    #[must_use]
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Returns true when the search found nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }
}

impl IntoIterator for SearchResponse {
    type Item = SearchMatch;
    type IntoIter = std::vec::IntoIter<SearchMatch>;

    fn into_iter(self) -> Self::IntoIter {
        self.results.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(request: &SearchRequest) -> serde_json::Value {
        serde_json::to_value(request).expect("serialize search request")
    }

    #[test]
    fn test_text_only_request_omits_optional_fields() {
        assert_eq!(
            body(&SearchRequest::new("tokyo")),
            serde_json::json!({"text": "tokyo"})
        );
    }

    #[test]
    fn test_full_request_serialization() {
        let request = SearchRequest::new("tokyo")
            .with_datasets(["app_messages"])
            .with_limit(3)
            .with_where("user_id = 42")
            .with_additional_columns(["timestamp"])
            .with_keywords(["plane", "tickets"]);

        assert_eq!(
            body(&request),
            serde_json::json!({
                "text": "tokyo",
                "datasets": ["app_messages"],
                "limit": 3,
                "where": "user_id = 42",
                "additional_columns": ["timestamp"],
                "keywords": ["plane", "tickets"],
            })
        );
    }

    #[test]
    fn test_empty_datasets_omitted() {
        // The runtime rejects an empty dataset list with a 400, so an empty
        // vector must be omitted rather than sent.
        let request = SearchRequest::new("tokyo").with_datasets(Vec::<String>::new());
        assert_eq!(body(&request), serde_json::json!({"text": "tokyo"}));
    }

    #[test]
    fn test_validate_rejects_empty_text() {
        let err = SearchRequest::new("   ")
            .validate()
            .expect_err("empty text should be rejected");
        assert!(err.to_string().contains("non-empty"), "{err}");
    }

    #[test]
    fn test_validate_rejects_zero_limit() {
        let err = SearchRequest::new("tokyo")
            .with_limit(0)
            .validate()
            .expect_err("zero limit should be rejected");
        assert!(err.to_string().contains("greater than 0"), "{err}");
    }

    #[test]
    fn test_validate_accepts_minimal_request() {
        SearchRequest::new("tokyo").validate().expect("valid");
    }

    #[test]
    fn test_response_deserialization() {
        let response: SearchResponse = serde_json::from_str(
            r#"{
                "results": [
                    {
                        "matches": {"message": ["I booked us some tickets", "direct to Narita"]},
                        "dataset": "app_messages",
                        "primary_key": {"id": "6fd5a215"},
                        "data": {"timestamp": 1724716542},
                        "metadata": {"chunk": 2},
                        "_score": 0.914321
                    },
                    {
                        "matches": {"message": ["we're sitting together"]},
                        "dataset": "app_messages",
                        "_score": 0.787654
                    }
                ],
                "duration_ms": 42
            }"#,
        )
        .expect("deserialize search response");

        assert_eq!(response.duration_ms, 42);
        assert_eq!(response.len(), 2);
        assert!(!response.is_empty());

        let first = &response.results[0];
        assert_eq!(first.dataset, "app_messages");
        assert!((first.score - 0.914_321).abs() < f64::EPSILON);
        // One column can contribute several chunks to a single match.
        assert_eq!(first.matches["message"].len(), 2);
        assert_eq!(first.primary_key["id"], "6fd5a215");
        assert_eq!(first.data["timestamp"], 1_724_716_542_i64);
        assert_eq!(first.metadata["chunk"], 2);

        // The runtime omits data, primary_key, and metadata when they are empty.
        let second = &response.results[1];
        assert!(second.primary_key.is_empty());
        assert!(second.data.is_empty());
        assert!(second.metadata.is_empty());
    }

    #[test]
    fn test_empty_response() {
        let response: SearchResponse =
            serde_json::from_str(r#"{"results": [], "duration_ms": 3}"#).expect("deserialize");
        assert!(response.is_empty());
        assert_eq!(response.into_iter().count(), 0);
    }
}
