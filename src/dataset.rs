use serde::{Deserialize, Serialize};
use snafu::Snafu;

#[derive(Debug, Snafu)]
pub enum DatasetError {
    #[snafu(display("Dataset not found: {dataset_name}"))]
    NotFound { dataset_name: String },

    #[snafu(display("Dataset {dataset_name} does not have acceleration enabled"))]
    AccelerationNotEnabled { dataset_name: String },

    #[snafu(display(
        "Failed to refresh dataset {dataset_name} (HTTP {status_code}): {response_body}"
    ))]
    RefreshFailed {
        dataset_name: String,
        status_code: u16,
        response_body: String,
    },

    #[snafu(display("Failed to refresh dataset {dataset_name}: {message}"))]
    HttpError {
        dataset_name: String,
        message: String,
    },

    #[snafu(display("Failed to parse dataset response for {dataset_name}: {message}"))]
    ParseError {
        dataset_name: String,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatasetRefreshMode {
    Full,
    Append,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DatasetRefreshRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_sql: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_mode: Option<DatasetRefreshMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_jitter_max: Option<String>,
}

impl DatasetRefreshRequest {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_refresh_sql(mut self, refresh_sql: impl Into<String>) -> Self {
        self.refresh_sql = Some(refresh_sql.into());
        self
    }

    #[must_use]
    pub fn with_refresh_mode(mut self, refresh_mode: DatasetRefreshMode) -> Self {
        self.refresh_mode = Some(refresh_mode);
        self
    }

    #[must_use]
    pub fn with_refresh_jitter_max(mut self, refresh_jitter_max: impl Into<String>) -> Self {
        self.refresh_jitter_max = Some(refresh_jitter_max.into());
        self
    }

    pub(crate) fn has_overrides(&self) -> bool {
        self.refresh_sql.is_some()
            || self.refresh_mode.is_some()
            || self.refresh_jitter_max.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DatasetRefreshResponse {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_refresh_request_default_has_no_overrides() {
        let request = DatasetRefreshRequest::new();
        assert!(!request.has_overrides());
    }

    #[test]
    fn test_refresh_request_builder() {
        let request = DatasetRefreshRequest::new()
            .with_refresh_sql("SELECT * FROM taxi_trips")
            .with_refresh_mode(DatasetRefreshMode::Full)
            .with_refresh_jitter_max("30s");

        assert!(request.has_overrides());
        assert_eq!(
            request.refresh_sql.as_deref(),
            Some("SELECT * FROM taxi_trips")
        );
        assert_eq!(request.refresh_mode, Some(DatasetRefreshMode::Full));
        assert_eq!(request.refresh_jitter_max.as_deref(), Some("30s"));
    }

    #[test]
    fn test_refresh_mode_serialization() {
        let serialized =
            serde_json::to_string(&DatasetRefreshMode::Append).expect("serialize refresh mode");
        assert_eq!(serialized, "\"append\"");

        let deserialized: DatasetRefreshMode =
            serde_json::from_str("\"full\"").expect("deserialize refresh mode");
        assert_eq!(deserialized, DatasetRefreshMode::Full);
    }
}
