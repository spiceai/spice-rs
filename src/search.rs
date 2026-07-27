use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::collections::HashMap;

#[derive(Debug, Snafu)]
pub enum SearchError {
    #[snafu(display("Search text cannot be empty"))]
    EmptyText,

    #[snafu(display(
        "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it. {message}"
    ))]
    HttpNotConfigured { message: String },

    #[snafu(display("Search request failed: {message}"))]
    HttpError { message: String },

    #[snafu(display("Search failed (HTTP {status_code}): {response_body}"))]
    SearchFailed {
        status_code: u16,
        response_body: String,
    },

    #[snafu(display("Failed to parse search response: {message}"))]
    ParseError { message: String },
}

/// A search over datasets that have an embedding column and a loaded embedding model.
///
/// Only the query text is required. Build with [`SearchRequest::new`] and refine
/// with the `with_*` methods:
///
/// ```
/// # use spiceai::SearchRequest;
/// let request = SearchRequest::new("tickets to Tokyo")
///     .with_datasets(["app_messages"])
///     .with_limit(3)
///     .with_keywords(["plane", "tickets"]);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SearchRequest {
    /// The query to find similar documents for.
    pub text: String,

    /// Restricts the search to these datasets. `None` searches every dataset
    /// with an embedding column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datasets: Option<Vec<String>>,

    /// Maximum matches to return per dataset. `None` uses the runtime's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// An SQL predicate applied before the search, without the `WHERE` keyword —
    /// for example `city = 'Tokyo'`.
    #[serde(rename = "where", skip_serializing_if = "Option::is_none")]
    pub where_cond: Option<String>,

    /// Extra dataset columns to return. A column that is part of the primary key
    /// is returned in [`SearchMatch::primary_key`] rather than [`SearchMatch::data`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_columns: Option<Vec<String>>,

    /// Pre-filters the embedding column with a lexical search before the vector
    /// search runs, making the search hybrid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
}

impl SearchRequest {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn with_datasets<I, S>(mut self, datasets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.datasets = Some(datasets.into_iter().map(Into::into).collect());
        self
    }

    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    #[must_use]
    pub fn with_where(mut self, where_cond: impl Into<String>) -> Self {
        self.where_cond = Some(where_cond.into());
        self
    }

    #[must_use]
    pub fn with_additional_columns<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.additional_columns = Some(columns.into_iter().map(Into::into).collect());
        self
    }

    #[must_use]
    pub fn with_keywords<I, S>(mut self, keywords: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.keywords = Some(keywords.into_iter().map(Into::into).collect());
        self
    }
}

/// A single document matched by a search.
///
/// The runtime omits `matches`, `primary_key`, `data`, and `metadata` when they
/// are empty; they deserialize to empty maps so they can be read without a guard.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SearchMatch {
    /// The dataset the match was found in.
    pub dataset: String,

    /// Similarity of the match to the query text. Higher is closer.
    #[serde(rename = "_score")]
    pub score: f64,

    /// The matched values of each searched column.
    #[serde(default)]
    pub matches: HashMap<String, Vec<serde_json::Value>>,

    /// Primary key identifying the matched row, if the dataset declares one.
    #[serde(default)]
    pub primary_key: HashMap<String, serde_json::Value>,

    /// Columns requested via [`SearchRequest::additional_columns`].
    #[serde(default)]
    pub data: HashMap<String, serde_json::Value>,

    /// Any additional metadata the runtime attached to the match.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// The result of a search.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SearchResponse {
    /// Matches, ordered by descending score.
    #[serde(default)]
    pub results: Vec<SearchMatch>,

    /// How long the runtime took to run the search.
    #[serde(default)]
    pub duration_ms: u64,
}

impl SearchResponse {
    /// Number of matches returned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Whether the search returned no matches.
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

impl<'a> IntoIterator for &'a SearchResponse {
    type Item = &'a SearchMatch;
    type IntoIter = std::slice::Iter<'a, SearchMatch>;

    fn into_iter(self) -> Self::IntoIter {
        self.results.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_text_only_omits_optional_fields() {
        let json = serde_json::to_value(SearchRequest::new("tickets to Tokyo"))
            .expect("serialize search request");
        assert_eq!(json, serde_json::json!({"text": "tickets to Tokyo"}));
    }

    #[test]
    fn test_request_builder() {
        let request = SearchRequest::new("tickets to Tokyo")
            .with_datasets(["app_messages"])
            .with_limit(3)
            .with_where("city = 'Tokyo'")
            .with_additional_columns(["timestamp"])
            .with_keywords(["plane", "tickets"]);

        let json = serde_json::to_value(&request).expect("serialize search request");
        assert_eq!(
            json,
            serde_json::json!({
                "text": "tickets to Tokyo",
                "datasets": ["app_messages"],
                "limit": 3,
                "where": "city = 'Tokyo'",
                "additional_columns": ["timestamp"],
                "keywords": ["plane", "tickets"],
            })
        );
    }

    #[test]
    fn test_where_uses_the_wire_field_name() {
        let json = serde_json::to_value(SearchRequest::new("x").with_where("a = 1"))
            .expect("serialize search request");
        assert!(json.get("where").is_some());
        assert!(json.get("where_cond").is_none());
    }

    #[test]
    fn test_response_deserializes_wire_format() {
        let body = serde_json::json!({
            "results": [
                {
                    "matches": {"message": ["I booked us some tickets"]},
                    "dataset": "app_messages",
                    "primary_key": {"id": "6fd5a215"},
                    "data": {"timestamp": 1_724_716_542_i64},
                    "_score": 0.914_321
                },
                {
                    "dataset": "app_messages",
                    "_score": 0.832_21
                }
            ],
            "duration_ms": 42
        });

        let response: SearchResponse =
            serde_json::from_value(body).expect("deserialize search response");

        assert_eq!(response.duration_ms, 42);
        assert_eq!(response.len(), 2);
        assert!(!response.is_empty());

        let first = &response.results[0];
        assert_eq!(first.dataset, "app_messages");
        assert!((first.score - 0.914_321).abs() < f64::EPSILON);
        assert_eq!(
            first
                .primary_key
                .get("id")
                .and_then(serde_json::Value::as_str),
            Some("6fd5a215")
        );
        assert_eq!(first.matches["message"].len(), 1);

        // Omitted objects deserialize to empty maps, readable without a guard.
        let second = &response.results[1];
        assert!(second.data.is_empty());
        assert!(second.primary_key.is_empty());
        assert!(second.metadata.is_empty());
        assert!(second.matches.is_empty());
    }

    #[test]
    fn test_response_iteration() {
        let body = serde_json::json!({
            "results": [
                {"dataset": "a", "_score": 0.9},
                {"dataset": "b", "_score": 0.8}
            ],
            "duration_ms": 1
        });
        let response: SearchResponse =
            serde_json::from_value(body).expect("deserialize search response");

        let datasets: Vec<&str> = (&response)
            .into_iter()
            .map(|m| m.dataset.as_str())
            .collect();
        assert_eq!(datasets, vec!["a", "b"]);

        let scores: Vec<f64> = response.into_iter().map(|m| m.score).collect();
        assert_eq!(scores.len(), 2);
    }

    #[test]
    fn test_empty_response() {
        let response: SearchResponse =
            serde_json::from_value(serde_json::json!({"results": [], "duration_ms": 0}))
                .expect("deserialize search response");
        assert!(response.is_empty());
        assert_eq!(response.len(), 0);
    }
}
