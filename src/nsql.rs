use serde::{Deserialize, Serialize};
use snafu::Snafu;

/// Errors returned when running a natural-language query.
#[derive(Debug, Snafu)]
pub enum NsqlError {
    /// The request was rejected before being sent.
    #[snafu(display("Invalid NSQL request: {message}"))]
    InvalidRequest { message: String },

    /// The runtime rejected the request.
    #[snafu(display("NSQL request failed (HTTP {status_code}): {response_body}"))]
    NsqlFailed {
        /// HTTP status code returned by the server.
        status_code: u16,
        /// Response body from the server.
        response_body: String,
    },

    /// HTTP transport error.
    #[snafu(display("NSQL request failed: {message}"))]
    HttpError { message: String },

    /// Failed to parse the server response.
    #[snafu(display("Failed to parse NSQL response: {message}"))]
    ParseError { message: String },
}

/// Media type that makes the runtime return the generated SQL alongside the
/// results. Without it `/v1/nsql` answers with a bare array of rows and the
/// generated SQL is lost.
pub(crate) const NSQL_JSON_MEDIA_TYPE: &str = "application/vnd.spiceai.nsql.v1+json";

/// Media type that makes the runtime generate SQL without executing it.
pub(crate) const NSQL_SQL_MEDIA_TYPE: &str = "application/sql";

fn is_false(value: &bool) -> bool {
    !*value
}

/// A natural-language query against the runtime's `/v1/nsql` endpoint.
///
/// Only the query text is required. The runtime needs an LLM model configured
/// in the Spicepod to translate it; when exactly one compatible model is
/// configured, the model may be left unset and the runtime selects it.
///
/// ```
/// use spiceai::NsqlRequest;
///
/// let request = NsqlRequest::new("top 5 customers by revenue")
///     .with_datasets(["sales"])
///     .with_sample_data(true);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NsqlRequest {
    /// The question to answer, in natural language.
    pub query: String,

    /// The LLM used to generate SQL. When unset, the runtime uses the only
    /// compatible model configured in the Spicepod, and reports an error if
    /// there is not exactly one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Datasets to sample when building the model's context. This is a
    /// sampling hint only — it does not restrict which tables the generated
    /// query may reference. When empty, all datasets are used.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub datasets: Vec<String>,

    /// Whether sample rows are included in the model's context. Improves
    /// generation on ambiguous schemas, at the cost of sending data values to
    /// the model.
    #[serde(skip_serializing_if = "is_false")]
    pub sample_data_enabled: bool,

    /// A stable key forwarded to the model provider for prompt caching. Reuse
    /// it across related requests to benefit from it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

impl NsqlRequest {
    /// Creates a request answering `query`.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Default::default()
        }
    }

    /// Names the model used to generate SQL.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Hints which datasets to sample when building the model's context.
    #[must_use]
    pub fn with_datasets<I, S>(mut self, datasets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.datasets = datasets.into_iter().map(Into::into).collect();
        self
    }

    /// Includes sample rows in the model's context.
    #[must_use]
    pub fn with_sample_data(mut self, enabled: bool) -> Self {
        self.sample_data_enabled = enabled;
        self
    }

    /// Sets the prompt cache key forwarded to the model provider.
    #[must_use]
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(key.into());
        self
    }

    /// Rejects requests the runtime would answer with a `400`, so the error
    /// names the field to fix rather than reporting a status code.
    pub(crate) fn validate(&self) -> Result<(), NsqlError> {
        if self.query.trim().is_empty() {
            return Err(NsqlError::InvalidRequest {
                message: "query is required and must be a non-empty natural language query"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// One column of an [`NsqlResponse`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NsqlField {
    /// The column name.
    #[serde(default)]
    pub name: String,

    /// The column's Arrow type, in the JSON encoding the runtime emits. Simple
    /// types appear as a string (`"Utf8"`), parameterized ones as an object
    /// (`{"Timestamp": ["Nanosecond", null]}`).
    #[serde(default)]
    pub data_type: serde_json::Value,

    /// Whether the column admits nulls.
    #[serde(default)]
    pub nullable: bool,
}

/// The schema of the rows an NSQL query returned.
///
/// `fields` is empty when the generated query returned no rows — the runtime
/// omits the schema body in that case.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct NsqlSchema {
    /// The columns, in order.
    #[serde(default)]
    pub fields: Vec<NsqlField>,
}

/// The result of running a natural-language query.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct NsqlResponse {
    /// The query the model generated. Worth logging: a surprising result is
    /// usually a surprising query.
    #[serde(default)]
    pub sql: String,

    /// The number of rows returned.
    #[serde(default)]
    pub row_count: usize,

    /// The columns in `data`.
    #[serde(default)]
    pub schema: NsqlSchema,

    /// The rows, each keyed by column name. Values are decoded from JSON, so
    /// they carry JSON's types rather than the Arrow types named in `schema`.
    /// Use [`crate::Client::nsql_generate_sql`] with [`crate::Client::query`]
    /// when Arrow-typed results matter.
    #[serde(default)]
    pub data: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl NsqlResponse {
    /// Returns the number of rows in `data`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns true when the generated query returned no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl IntoIterator for NsqlResponse {
    type Item = serde_json::Map<String, serde_json::Value>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(request: &NsqlRequest) -> serde_json::Value {
        serde_json::to_value(request).expect("serialize nsql request")
    }

    #[test]
    fn test_query_only_request_omits_optional_fields() {
        assert_eq!(
            body(&NsqlRequest::new("how many orders")),
            serde_json::json!({"query": "how many orders"})
        );
    }

    #[test]
    fn test_full_request_serialization() {
        let request = NsqlRequest::new("top 5 customers by revenue")
            .with_model("nsql-model")
            .with_datasets(["sales"])
            .with_sample_data(true)
            .with_prompt_cache_key("sales-dashboard");

        assert_eq!(
            body(&request),
            serde_json::json!({
                "query": "top 5 customers by revenue",
                "model": "nsql-model",
                "datasets": ["sales"],
                "sample_data_enabled": true,
                "prompt_cache_key": "sales-dashboard",
            })
        );
    }

    #[test]
    fn test_defaults_omitted() {
        // sample_data_enabled defaults to false server-side, so sending it
        // adds nothing; an empty dataset list is a sampling hint of "all".
        let request = NsqlRequest::new("how many orders")
            .with_datasets(Vec::<String>::new())
            .with_sample_data(false);
        assert_eq!(
            body(&request),
            serde_json::json!({"query": "how many orders"})
        );
    }

    #[test]
    fn test_validate_rejects_empty_query() {
        let err = NsqlRequest::new("   ")
            .validate()
            .expect_err("empty query should be rejected");
        assert!(err.to_string().contains("non-empty"), "{err}");
    }

    #[test]
    fn test_validate_accepts_minimal_request() {
        NsqlRequest::new("how many orders")
            .validate()
            .expect("valid");
    }

    #[test]
    fn test_response_deserialization() {
        let response: NsqlResponse = serde_json::from_str(
            r#"{
                "row_count": 2,
                "schema": {
                    "fields": [
                        {"name": "customer_id", "data_type": "Utf8", "nullable": false},
                        {"name": "ts", "data_type": {"Timestamp": ["Nanosecond", null]}, "nullable": true}
                    ]
                },
                "data": [
                    {"customer_id": "12345", "ts": 1724716542},
                    {"customer_id": "67890", "ts": 1724716543}
                ],
                "sql": "SELECT customer_id, ts FROM sales LIMIT 2"
            }"#,
        )
        .expect("deserialize nsql response");

        assert_eq!(response.sql, "SELECT customer_id, ts FROM sales LIMIT 2");
        assert_eq!(response.row_count, 2);
        assert_eq!(response.len(), 2);
        assert!(!response.is_empty());
        assert_eq!(response.data[0]["customer_id"], "12345");

        assert_eq!(response.schema.fields.len(), 2);
        assert_eq!(response.schema.fields[0].name, "customer_id");
        // A simple Arrow type encodes as a string, a parameterized one as an
        // object, which is why data_type stays a serde_json::Value.
        assert_eq!(response.schema.fields[0].data_type, "Utf8");
        assert_eq!(
            response.schema.fields[1].data_type,
            serde_json::json!({"Timestamp": ["Nanosecond", null]})
        );
        assert!(response.schema.fields[1].nullable);
    }

    #[test]
    fn test_empty_result_set() {
        // The runtime serializes schema as {} when the query returned no rows,
        // so deserialization must not depend on a fields key being present.
        let response: NsqlResponse = serde_json::from_str(
            r#"{"row_count": 0, "schema": {}, "data": [], "sql": "SELECT 1 WHERE false"}"#,
        )
        .expect("deserialize");

        assert!(response.is_empty());
        assert!(response.schema.fields.is_empty());
        assert_eq!(response.sql, "SELECT 1 WHERE false");
        assert_eq!(response.into_iter().count(), 0);
    }
}
