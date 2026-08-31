use crate::active_query::{ActiveQueryError, ActiveQueryList, CancelActiveQueryResponse};
use crate::flight::RetryableQueryStream;
use crate::params::{QueryParameterError, QueryParameters};
use crate::query::{QueryError, QueryHttpClient, QueryJob, QuerySubmitOptions};
use crate::util::{FibonacciBackoffBuilder, RetryError, retry};
use crate::{
    config::{GenericError, SPICE_CLOUD_FLIGHT_ADDR, SPICE_LOCAL_FLIGHT_ADDR},
    dataset::{DatasetError, DatasetRefreshRequest, DatasetRefreshResponse},
    flight::{SqlFlightClient, is_connection_reset_generic_error},
    nsql::{NsqlContextRequest, NsqlError, NsqlRequest, NsqlResponse},
    search::{SearchError, SearchRequest, SearchResponse},
    status::{ConnectionDetails, StatusError},
    tls::{FlightChannelBuilder, ensure_crypto_provider, new_tls_flight_channel},
};
use arrow::record_batch::RecordBatch;
use arrow_flight::error::FlightError;
use snafu::Snafu;
use std::sync::Arc;

use tonic::transport::Channel;

const MAX_RETRIES: u32 = 3;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Query execution failed: {source}"))]
    Query {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("Failed to build query parameters: {source}"))]
    ParameterBindings { source: QueryParameterError },

    #[snafu(display("Failed to process query stream: {source}"))]
    QueryStream { source: FlightError },

    #[snafu(display("Connection reset: {message}\nPlease retry the query."))]
    ConnectionReset { message: String },
}

/// Classifies the error a retry loop gave up on.
///
/// A reset that outlived the retries is still a reset. Reporting it as a plain query
/// failure discards the one thing that tells a caller the failure is transient, and the
/// classification cannot be recovered downstream: the status is boxed inside an error
/// type the caller may not share a version of.
fn exhausted_retry_error(error: GenericError) -> Error {
    if is_connection_reset_generic_error(&error) {
        return Error::ConnectionReset {
            message: error.to_string(),
        };
    }
    Error::Query { source: error }
}

struct SpiceClientConfig {
    flight_channel: Channel,
}

impl SpiceClientConfig {
    fn new(flight_channel: Channel) -> Self {
        SpiceClientConfig { flight_channel }
    }

    pub async fn load_from_default() -> Result<SpiceClientConfig, GenericError> {
        let flight_chan = new_tls_flight_channel(SPICE_CLOUD_FLIGHT_ADDR).await?;

        Ok(SpiceClientConfig::new(flight_chan))
    }
}

/// The `SpiceClient` is the main entry point for interacting with Spice.
/// It provides methods for Flight SQL queries and runtime HTTP APIs.
#[allow(clippy::module_name_repetitions)]
#[derive(Clone)]
pub struct SpiceClient {
    flight: Arc<SqlFlightClient>,
    http_client: Option<Arc<QueryHttpClient>>,
}

impl SpiceClient {
    /// Creates a new `SpiceClient` with the given API key and default user agent.
    /// ```
    /// use spiceai::Client;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let client = Client::new("API_KEY").await.unwrap();
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` for any query error
    pub async fn new(api_key: &str) -> Result<Self, GenericError> {
        ensure_crypto_provider();
        let config = SpiceClientConfig::load_from_default().await?;

        Ok(Self {
            flight: Arc::new(SqlFlightClient::new(
                config.flight_channel,
                Some(api_key.to_string()),
                None,
                None,
                MAX_RETRIES,
            )),
            http_client: None,
        })
    }

    #[must_use]
    pub fn builder() -> SpiceClientBuilder {
        SpiceClientBuilder::new()
    }

    /// Executes a synchronous SQL query against the Spice Flight endpoint.
    ///
    /// This method executes the query immediately and returns a stream of record batches.
    /// For long-running queries, consider using [`query()`](Self::query) for async execution.
    ///
    /// ```
    /// # use spiceai::Client;
    /// # #[tokio::main]
    /// # async fn main() {
    /// #  let client = Client::new("API_KEY").await.unwrap();
    /// #  let data = client.sql("SELECT * FROM taxi_trips LIMIT 10;").await;
    /// # }
    /// ````
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` for any query error
    pub async fn sql(&self, query: &str) -> Result<RetryableQueryStream, Error> {
        let retry_strategy = FibonacciBackoffBuilder::new()
            .max_retries(Some(MAX_RETRIES as usize))
            .build();

        retry(retry_strategy, || async {
            match self.flight.query(query).await {
                Ok(stream) => Ok(RetryableQueryStream::new(
                    Arc::clone(&self.flight),
                    query,
                    None,
                    Box::pin(stream),
                )),
                Err(e) => {
                    if is_connection_reset_generic_error(&e) {
                        return Err(RetryError::transient(e));
                    }
                    Err(RetryError::Permanent(e))
                }
            }
        })
        .await
        .map_err(exhausted_retry_error)
    }

    /// Executes a synchronous parameterized SQL query against the Spice Flight endpoint.
    ///
    /// If `params` is `None`, it behaves like [`sql()`](Self::sql).
    /// `params` is a parameter binding `RecordBatch`.
    /// <https://docs.rs/arrow-flight/latest/arrow_flight/sql/client/struct.PreparedStatement.html#method.set_parameters>
    ///
    /// For long-running queries, consider using [`query()`](Self::query) for async execution.
    ///
    /// ```
    /// # use spiceai::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #  let client = Client::new("API_KEY").await.unwrap();
    /// #  let data = client.sql_with_params("SELECT * FROM taxi_trips LIMIT 10;", None).await;
    /// # }
    /// ````
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` for any query error
    pub async fn sql_with_params(
        &self,
        query: &str,
        params: Option<RecordBatch>,
    ) -> Result<RetryableQueryStream, Error> {
        let retry_strategy = FibonacciBackoffBuilder::new()
            .max_retries(Some(MAX_RETRIES as usize))
            .build();

        retry(retry_strategy, || async {
            match self.flight.query_with_params(query, params.clone()).await {
                Ok(stream) => Ok(RetryableQueryStream::new(
                    Arc::clone(&self.flight),
                    query,
                    params.clone(),
                    Box::pin(stream),
                )),
                Err(e) => {
                    if is_connection_reset_generic_error(&e) {
                        return Err(RetryError::transient(e));
                    }
                    Err(RetryError::Permanent(e))
                }
            }
        })
        .await
        .map_err(exhausted_retry_error)
    }

    /// Executes a synchronous parameterized SQL query using scalar bindings.
    ///
    /// This is a convenience wrapper over [`sql_with_params()`](Self::sql_with_params)
    /// for the common case of binding a single row of scalar values. For advanced
    /// Arrow parameter batches, use [`sql_with_params()`](Self::sql_with_params).
    ///
    /// ```
    /// # use spiceai::{Client, QueryParameters};
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #  let client = Client::new("API_KEY").await.unwrap();
    /// #  let _ = client
    /// #    .sql_with_bindings(
    /// #      "SELECT * FROM taxi_trips WHERE VendorID = $1 AND fare_amount > $2 LIMIT 10;",
    /// #      QueryParameters::new().push(1_i32).push(1.0_f64),
    /// #    )
    /// #    .await;
    /// # }
    /// ```
    ///
    /// ## Errors
    ///
    /// - [`Error::ParameterBindings`] if the bindings cannot be converted into an Arrow batch
    /// - [`Error::Query`] for query execution errors
    pub async fn sql_with_bindings(
        &self,
        query: &str,
        params: QueryParameters,
    ) -> Result<RetryableQueryStream, Error> {
        let params = params
            .into_record_batch()
            .map_err(|source| Error::ParameterBindings { source })?;

        match params {
            Some(params) => self.sql_with_params(query, Some(params)).await,
            None => self.sql(query).await,
        }
    }

    /// Submits an async SQL query and returns a [`QueryJob`] handle.
    ///
    /// This method submits the query to the `/v1/queries` API for asynchronous execution
    /// and returns immediately with a job handle. Use the returned [`QueryJob`] to:
    /// - Check status with [`QueryJob::status()`]
    /// - Wait for completion with [`QueryJob::wait()`] or [`QueryJob::wait_timeout()`]
    /// - Retrieve results with [`QueryJob::results()`]
    /// - Cancel the query with [`QueryJob::cancel()`]
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    /// Async queries require cluster mode with `scheduler.state_location` configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// // Submit async query
    /// let job = client.query("SELECT * FROM large_table").await?;
    /// println!("Query submitted: {}", job.id());
    ///
    /// // Wait for completion
    /// let result = job.wait().await?;
    /// println!("Completed with {} rows", result.total_rows);
    ///
    /// // Get results as Arrow record batches
    /// let batches = job.results().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`QueryError::ClusterModeRequired`] if async queries are not enabled
    /// - [`QueryError::SubmitFailed`] if the query submission fails
    /// - [`QueryError::HttpError`] if the HTTP endpoint is not configured or unreachable
    pub async fn query(&self, sql: &str) -> Result<QueryJob, QueryError> {
        let http_client = self.http_client.as_ref().ok_or(QueryError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        let response = http_client.submit(sql, None, None, None).await?;
        Ok(QueryJob::new(response.query_id, Arc::clone(http_client)))
    }

    /// Submits an async parameterized SQL query using scalar bindings.
    ///
    /// This is the async (`/v1/queries`) counterpart to
    /// [`sql_with_bindings()`](Self::sql_with_bindings): it binds a single row of
    /// positional scalar values (`$1`, `$2`, ...) and returns a [`QueryJob`]
    /// handle. To also set a timeout or result-size cap, use
    /// [`query_with_options()`](Self::query_with_options).
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::{ClientBuilder, QueryParameters};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let job = client
    ///     .query_with_bindings(
    ///         "SELECT * FROM large_table WHERE status = $1",
    ///         QueryParameters::new().push("active"),
    ///     )
    ///     .await?;
    /// println!("Query submitted: {}", job.id());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`QueryError::InvalidParameter`] if a binding cannot be encoded as JSON
    /// - [`QueryError::ClusterModeRequired`] if async queries are not enabled
    /// - [`QueryError::SubmitFailed`] if the query submission fails
    /// - [`QueryError::HttpError`] if the HTTP endpoint is not configured or unreachable
    pub async fn query_with_bindings(
        &self,
        sql: &str,
        params: QueryParameters,
    ) -> Result<QueryJob, QueryError> {
        self.query_with_options(sql, QuerySubmitOptions::new().bindings(params))
            .await
    }

    /// Submits an async SQL query with explicit submit options.
    ///
    /// [`QuerySubmitOptions`] carries the optional bind parameters, a server-side
    /// execution `timeout_seconds`, and a `maximum_size` cap on the materialized
    /// result. Any option left unset is omitted from the request so the server
    /// applies its default.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::{ClientBuilder, QueryParameters, QuerySubmitOptions};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let job = client
    ///     .query_with_options(
    ///         "SELECT * FROM large_table WHERE status = $1",
    ///         QuerySubmitOptions::new()
    ///             .bindings(QueryParameters::new().push("active"))
    ///             .timeout_seconds(300)
    ///             .maximum_size(100_000_000),
    ///     )
    ///     .await?;
    /// let result = job.wait().await?;
    /// println!("Completed with {} rows", result.total_rows);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`QueryError::InvalidParameter`] if a binding cannot be encoded as JSON
    /// - [`QueryError::ClusterModeRequired`] if async queries are not enabled
    /// - [`QueryError::SubmitFailed`] if the query submission fails
    /// - [`QueryError::HttpError`] if the HTTP endpoint is not configured or unreachable
    pub async fn query_with_options(
        &self,
        sql: &str,
        options: QuerySubmitOptions,
    ) -> Result<QueryJob, QueryError> {
        let http_client = self.http_client.as_ref().ok_or(QueryError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        let parameters = match &options.bindings {
            Some(params) if !params.is_empty() => Some(
                params
                    .to_json_values()
                    .map_err(|source| QueryError::InvalidParameter { source })?,
            ),
            _ => None,
        };

        let response = http_client
            .submit(
                sql,
                parameters,
                options.timeout_seconds,
                options.maximum_size,
            )
            .await?;
        Ok(QueryJob::new(response.query_id, Arc::clone(http_client)))
    }

    /// Lists async queries on the server.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Arguments
    ///
    /// * `status_filter` - Optional filter by status: "pending", "running", "succeeded", "failed", "cancelled"
    /// * `limit` - Optional maximum number of queries to return (default: 100)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// // List all queries
    /// let queries = client.queries(None, None).await?;
    /// for q in &queries.queries {
    ///     println!("{}: {} - {}", q.query_id, q.status, q.sql_preview);
    /// }
    ///
    /// // List only running queries
    /// let running = client.queries(Some("running"), Some(10)).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`QueryError::ClusterModeRequired`] if async queries are not enabled
    /// - [`QueryError::HttpError`] if the HTTP endpoint is not configured or unreachable
    pub async fn queries(
        &self,
        status_filter: Option<&str>,
        limit: Option<usize>,
    ) -> Result<crate::query::QueryListResponse, QueryError> {
        let http_client = self.http_client.as_ref().ok_or(QueryError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        http_client.list_queries(status_filter, limit).await
    }

    /// Gets a [`QueryJob`] handle for an existing query by ID.
    ///
    /// This allows you to resume tracking a query that was submitted earlier,
    /// check its status, retrieve results, or cancel it.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// // Get a handle for an existing query
    /// let job = client.get_query("qry_abc123")?;
    ///
    /// // Check its status
    /// let status = job.status().await?;
    /// println!("Status: {}", status);
    ///
    /// // Wait and get results if still running
    /// if status.is_running() {
    ///     let result = job.wait().await?;
    ///     let batches = job.results().await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`QueryError::HttpError`] if the HTTP endpoint is not configured
    pub fn get_query(&self, query_id: &str) -> Result<QueryJob, QueryError> {
        let http_client = self.http_client.as_ref().ok_or(QueryError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        Ok(QueryJob::new(query_id.to_string(), Arc::clone(http_client)))
    }

    /// Cancels an async query by ID.
    ///
    /// This is a convenience method equivalent to calling [`get_query()`](Self::get_query)
    /// followed by [`QueryJob::cancel()`].
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// // Cancel a query by ID
    /// let info = client.cancel_query("qry_abc123").await?;
    /// println!("Query {} cancelled", info.query_id);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`QueryError::NotFound`] if the query does not exist
    /// - [`QueryError::HttpError`] if the query has already completed or the endpoint is not configured
    pub async fn cancel_query(
        &self,
        query_id: &str,
    ) -> Result<crate::query::QueryInfo, QueryError> {
        let http_client = self.http_client.as_ref().ok_or(QueryError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        let job = QueryJob::new(query_id.to_string(), Arc::clone(http_client));
        job.cancel().await
    }

    /// Lists the *synchronous* queries this client currently has running.
    ///
    /// Synchronous queries are the ones started by [`sql()`](Self::sql), FlightSQL,
    /// `/v1/sql`, NSQL, and search. For async query jobs, use
    /// [`queries()`](Self::queries) instead.
    ///
    /// The runtime does not return a query's id to the client that submitted it,
    /// so this is how you find the id needed by
    /// [`cancel_active_query()`](Self::cancel_active_query).
    ///
    /// Results cover one runtime instance — the one [`http_url()`](SpiceClientBuilder::http_url)
    /// addresses — and are scoped to the **authenticated principal**, not to this
    /// client: every client presenting the same credential sees the same queries.
    /// See [`active_query`](crate::active_query) for the full boundary.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let active = client.active_queries().await?;
    /// println!("{} queries running", active.total_count);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`ActiveQueryError::WriteAccessRequired`] if the API key lacks write access
    /// - [`ActiveQueryError::HttpError`] if the HTTP endpoint is not configured or unreachable
    pub async fn active_queries(&self) -> Result<ActiveQueryList, ActiveQueryError> {
        let http_client = self
            .http_client
            .as_ref()
            .ok_or(ActiveQueryError::HttpError {
                message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                    .to_string(),
            })?;

        http_client.list_active_queries().await
    }

    /// Cancels a running *synchronous* query by id.
    ///
    /// Takes a `query_id` from [`active_queries()`](Self::active_queries). To cancel
    /// an async query job instead, use [`cancel_query()`](Self::cancel_query).
    ///
    /// Cancellation reaches the runtime instance [`http_url()`](SpiceClientBuilder::http_url)
    /// addresses, and is scoped to the **authenticated principal**, not to this
    /// client: any client presenting the same credential can cancel the query, while
    /// an id outside that scope is reported as not found rather than cancelled.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let active = client.active_queries().await?;
    /// for query in active.queries {
    ///     client.cancel_active_query(&query.query_id).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`ActiveQueryError::NotFound`] if no such query is running in the caller's scope
    /// - [`ActiveQueryError::InvalidQueryId`] if `query_id` is not a UUID
    /// - [`ActiveQueryError::WriteAccessRequired`] if the API key lacks write access
    pub async fn cancel_active_query(
        &self,
        query_id: &str,
    ) -> Result<CancelActiveQueryResponse, ActiveQueryError> {
        let http_client = self
            .http_client
            .as_ref()
            .ok_or(ActiveQueryError::HttpError {
                message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                    .to_string(),
            })?;

        http_client.cancel_active_query(query_id).await
    }

    /// Triggers an on-demand refresh for an accelerated dataset.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let response = client.refresh_dataset("taxi_trips").await?;
    /// println!("{}", response.message);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn refresh_dataset(
        &self,
        dataset_name: &str,
    ) -> Result<DatasetRefreshResponse, DatasetError> {
        self.refresh_dataset_with_options(dataset_name, DatasetRefreshRequest::default())
            .await
    }

    /// Triggers an on-demand refresh for an accelerated dataset with overrides.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    pub async fn refresh_dataset_with_options(
        &self,
        dataset_name: &str,
        request: DatasetRefreshRequest,
    ) -> Result<DatasetRefreshResponse, DatasetError> {
        let http_client = self.http_client.as_ref().ok_or(DatasetError::HttpError {
            dataset_name: dataset_name.to_string(),
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        http_client.refresh_dataset(dataset_name, &request).await
    }

    /// Searches datasets for documents similar to the request's text.
    ///
    /// Runs against datasets that have an embedding column and a loaded
    /// embedding model — see [Search & Retrieval](https://docs.spice.ai/features/search-and-retrieval)
    /// for how to configure them. Adding keywords to the request turns this
    /// into a hybrid search, combining a lexical pass with the vector scores.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::{ClientBuilder, SearchRequest};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let response = client
    ///     .search(
    ///         SearchRequest::new("tokyo plane tickets")
    ///             .with_datasets(["app_messages"])
    ///             .with_limit(3),
    ///     )
    ///     .await?;
    ///
    /// for m in response {
    ///     println!("{} {} {:?}", m.score, m.dataset, m.matches);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`SearchError::InvalidRequest`] if the request has empty text or a zero limit
    /// - [`SearchError::HttpError`] if the HTTP endpoint is not configured or unreachable
    /// - [`SearchError::SearchFailed`] if the runtime rejects the search
    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse, SearchError> {
        let http_client = self.http_client.as_ref().ok_or(SearchError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        http_client.search(&request).await
    }

    /// Answers a natural-language question by generating SQL and running it.
    ///
    /// Backed by `POST /v1/nsql`: the configured LLM translates the question,
    /// the runtime executes the result read-only, and both the rows and the
    /// generated SQL come back. Requires an LLM model in the Spicepod — see
    /// [Text to SQL](https://docs.spice.ai/features/text-to-sql) for how to
    /// configure one.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::{ClientBuilder, NsqlRequest};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let response = client
    ///     .nsql(NsqlRequest::new("top 5 customers by revenue").with_datasets(["sales"]))
    ///     .await?;
    ///
    /// println!("generated SQL: {}", response.sql);
    /// for row in response {
    ///     println!("{row:?}");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`NsqlError::InvalidRequest`] if the request has an empty query
    /// - [`NsqlError::HttpError`] if the HTTP endpoint is not configured or unreachable
    /// - [`NsqlError::NsqlFailed`] if the runtime rejects the request, which is
    ///   what a missing or ambiguous model reports as
    pub async fn nsql(&self, request: NsqlRequest) -> Result<NsqlResponse, NsqlError> {
        let http_client = self.http_client.as_ref().ok_or(NsqlError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        http_client.nsql(&request).await
    }

    /// Translates a natural-language question into SQL without running it.
    ///
    /// Use it to inspect or edit the query before running it, or to run it
    /// through [`query`](Self::query) or the Flight path so results arrive as
    /// Arrow rather than decoded JSON.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::{ClientBuilder, NsqlRequest};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let sql = client
    ///     .nsql_generate_sql(NsqlRequest::new("top 5 customers by revenue"))
    ///     .await?;
    ///
    /// println!("{sql}");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Same as [`nsql`](Self::nsql).
    pub async fn nsql_generate_sql(&self, request: NsqlRequest) -> Result<String, NsqlError> {
        let http_client = self.http_client.as_ref().ok_or(NsqlError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        http_client.nsql_generate_sql(&request).await
    }

    /// Returns the context block `/v1/nsql` builds for its model.
    ///
    /// Backed by `GET /v1/nsql/context`. The block is markdown listing the
    /// in-scope datasets and their schemas, the SQL functions available, and
    /// optionally sample rows — exactly what [`nsql`](Self::nsql) sends the
    /// model. Use it to see why a generated query was wrong, or to reuse the
    /// runtime's schema context in a prompt of your own.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use spiceai::{ClientBuilder, NsqlContextRequest};
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// let context = client
    ///     .nsql_context(NsqlContextRequest::new().with_datasets(["sales.orders"]))
    ///     .await?;
    ///
    /// println!("{context}");
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`NsqlError::InvalidRequest`] when a limit exceeds the runtime's
    /// maximum, and otherwise the same errors as [`nsql`](Self::nsql).
    pub async fn nsql_context(&self, request: NsqlContextRequest) -> Result<String, NsqlError> {
        let http_client = self.http_client.as_ref().ok_or(NsqlError::HttpError {
            message: "HTTP endpoint not configured. Use ClientBuilder::http_url() to set it."
                .to_string(),
        })?;

        http_client.nsql_context(&request).await
    }

    /// Returns the status of each runtime connection.
    ///
    /// Backed by `GET /v1/status`. Where [`is_ready`](Self::is_ready) reports a single
    /// boolean for the whole runtime, this reports `http`, `flight`, `metrics` and
    /// `opentelemetry` individually, so it can say *which* component is not ready.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    ///
    /// ```no_run
    /// # use spiceai::ClientBuilder;
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    /// let client = ClientBuilder::new()
    ///     .http_url("http://localhost:8090")
    ///     .build()
    ///     .await?;
    ///
    /// for component in client.runtime_status().await? {
    ///     println!("{} ({}): {}", component.name, component.endpoint, component.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn runtime_status(&self) -> Result<Vec<ConnectionDetails>, StatusError> {
        let http_client = self
            .http_client
            .as_ref()
            .ok_or(StatusError::HttpNotConfigured)?;

        http_client.runtime_status().await
    }

    /// Returns whether the runtime is ready to serve queries.
    ///
    /// Backed by `GET /v1/ready`. Returns `Ok(false)` when the runtime responds that it
    /// is not ready; an `Err` means the probe itself could not be completed.
    ///
    /// **Note:** Requires [`http_url()`](SpiceClientBuilder::http_url) to be configured.
    pub async fn is_ready(&self) -> Result<bool, StatusError> {
        let http_client = self
            .http_client
            .as_ref()
            .ok_or(StatusError::HttpNotConfigured)?;

        http_client.is_ready().await
    }
}

/// Builder for creating a `SpiceClient`.
///
/// By default the `SpiceClient` will use local spice runtime flight endpoint.
/// Follow [spiceai quickstart](https://github.com/spiceai/spiceai?tab=readme-ov-file#%EF%B8%8F-quickstart-local-machine) to setup local spice runtime.
/// ```no_run
/// # use spiceai::ClientBuilder;
///
/// # #[tokio::main]
/// # async fn main() {
/// #    let client = ClientBuilder::new()
/// #      .build()
/// #      .await
/// #      .unwrap();
/// # }
/// ```
/// To use default Spice.ai Cloud endpoints, you can use the `with_spiceai_cloud()` method.
///
/// ```
/// # use spiceai::ClientBuilder;
/// # #[tokio::main]
/// # async fn main() {
/// #    let client = ClientBuilder::new()
/// #      .api_key("API_KEY")
/// #      .use_spiceai_cloud()
/// #      .build()
/// #      .await
/// #      .unwrap();
/// # }
/// ```
///
pub struct SpiceClientBuilder {
    api_key: Option<String>,
    user_agent: Option<String>,
    flight_url: Option<String>,
    http_url: Option<String>,
    cache_control: Option<String>,
    max_retries: u32,
    tls_client_certificate_file: Option<String>,
    tls_client_key_file: Option<String>,
    tls_ca_certificate_file: Option<String>,
}

impl Default for SpiceClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SpiceClientBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            api_key: None,
            user_agent: None,
            flight_url: None,
            http_url: None,
            cache_control: None,
            max_retries: MAX_RETRIES,
            tls_client_certificate_file: None,
            tls_client_key_file: None,
            tls_ca_certificate_file: None,
        }
    }

    /// Configures the `SpiceClient` to use the given API key.
    #[must_use]
    pub fn api_key(mut self, api_key: &str) -> Self {
        self.api_key = Some(api_key.to_string());
        self
    }

    /// Configures the `SpiceClient` to use the given custom user agent.
    #[must_use]
    pub fn user_agent(mut self, user_agent: &str) -> Self {
        self.user_agent = Some(user_agent.to_string());
        self
    }

    /// Configures the `SpiceClient` to use the given Spice Flight endpoint.
    #[must_use]
    pub fn flight_url(mut self, flight_url: &str) -> Self {
        self.flight_url = Some(flight_url.to_string());
        self
    }

    /// Configures the `SpiceClient` to use the given maximum number of retries.
    #[must_use]
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Configures the cache control to use the given cache control policy.
    #[must_use]
    pub fn cache_control(mut self, cache_control: &str) -> Self {
        self.cache_control = Some(cache_control.to_string());
        self
    }

    /// Configures the `SpiceClient` to use default Spice.ai Cloud endpoints.
    /// Equivalent to calling `.flight_url("https://flight.spiceai.io")`.
    #[must_use]
    pub fn use_spiceai_cloud(mut self) -> Self {
        self.flight_url = Some(SPICE_CLOUD_FLIGHT_ADDR.to_string());
        self
    }

    /// Configures the HTTP endpoint for runtime HTTP APIs.
    ///
    /// This endpoint is required for using async query management and dataset refresh APIs.
    /// Typically this is `http://localhost:8090` for local development or the
    /// HTTP API endpoint for your Spice.ai cluster.
    #[must_use]
    pub fn http_url(mut self, http_url: &str) -> Self {
        self.http_url = Some(http_url.to_string());
        self
    }

    /// Sets the path to a PEM-encoded client certificate file for mTLS.
    /// Must be used together with [`tls_client_key_file`](Self::tls_client_key_file).
    #[must_use]
    pub fn tls_client_certificate_file(mut self, path: &str) -> Self {
        self.tls_client_certificate_file = Some(path.to_string());
        self
    }

    /// Sets the path to a PEM-encoded client private key file for mTLS.
    /// Must be used together with [`tls_client_certificate_file`](Self::tls_client_certificate_file).
    #[must_use]
    pub fn tls_client_key_file(mut self, path: &str) -> Self {
        self.tls_client_key_file = Some(path.to_string());
        self
    }

    /// Sets the path to a custom CA certificate file for server verification.
    /// When set, this CA is used instead of the system certificate store.
    #[must_use]
    pub fn tls_ca_certificate_file(mut self, path: &str) -> Self {
        self.tls_ca_certificate_file = Some(path.to_string());
        self
    }

    /// Builds the `SpiceClient` with the specified configuration.
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` if flight channel creation fails
    pub async fn build(self) -> Result<SpiceClient, GenericError> {
        ensure_crypto_provider();

        // Validate that client cert and key are either both set or both unset
        match (&self.tls_client_certificate_file, &self.tls_client_key_file) {
            (Some(_), None) => {
                return Err("tls_client_certificate_file is set but tls_client_key_file is missing; both must be provided together for mTLS".into());
            }
            (None, Some(_)) => {
                return Err("tls_client_key_file is set but tls_client_certificate_file is missing; both must be provided together for mTLS".into());
            }
            _ => {}
        }

        let url = self
            .flight_url
            .as_deref()
            .unwrap_or(SPICE_LOCAL_FLIGHT_ADDR);

        let mut channel_builder = FlightChannelBuilder::new(url);
        if let (Some(cert), Some(key)) =
            (&self.tls_client_certificate_file, &self.tls_client_key_file)
        {
            channel_builder = channel_builder.with_client_certificate(cert, key);
        }
        if let Some(ca) = &self.tls_ca_certificate_file {
            channel_builder = channel_builder.with_ca_certificate(ca);
        }
        let flight_channel = channel_builder.build().await?;

        let http_client = if let Some(url) = self.http_url {
            // Built from the shared helper so the API key cannot follow a redirect off the
            // origin it was configured for (#12502).
            let mut builder = crate::redirect::credentialed_client_builder();
            if let (Some(cert_path), Some(key_path)) =
                (&self.tls_client_certificate_file, &self.tls_client_key_file)
            {
                let cert_pem = tokio::fs::read(cert_path).await?;
                let key_pem = tokio::fs::read(key_path).await?;
                let identity = reqwest::Identity::from_pem(&[cert_pem, key_pem].concat())?;
                builder = builder.identity(identity);
            }
            if let Some(ca_path) = &self.tls_ca_certificate_file {
                let ca_pem = tokio::fs::read(ca_path).await?;
                let ca = reqwest::Certificate::from_pem(&ca_pem)?;
                builder = builder.add_root_certificate(ca);
            }
            Some(Arc::new(QueryHttpClient::with_client(
                builder.build()?,
                &url,
                self.api_key.clone(),
            )))
        } else {
            None
        };

        Ok(SpiceClient {
            flight: Arc::new(SqlFlightClient::new(
                flight_channel,
                self.api_key.clone(),
                self.user_agent.clone(),
                self.cache_control.clone(),
                self.max_retries,
            )),
            http_client,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::DatasetRefreshMode;
    use crate::query::QueryStatus;
    use arrow::array::Int32Array;
    use futures::TryStreamExt;
    use serde_json::json;
    use std::time::Duration;
    use tonic::transport::Endpoint;
    use wiremock::matchers::{body_json, header, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Accepts connections and resets them, so every attempt a retry loop makes fails
    /// the same way and the budget runs out.
    async fn always_reset_endpoint() -> String {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    // Let the client send its preface first, so this is a reset rather
                    // than a refused connection.
                    let _ = socket.read(&mut [0u8; 1024]).await;
                    let _ = socket.set_linger(Some(Duration::ZERO));
                    drop(socket);
                });
            }
        });

        format!("http://{addr}")
    }

    /// Exercises the wiring, not just the classifier: the reset has to survive the retry
    /// loop in `sql` itself.
    #[tokio::test]
    async fn sql_reports_an_exhausted_reset_as_connection_reset() {
        let url = always_reset_endpoint().await;
        let client = SpiceClient::builder()
            .flight_url(&url)
            .build()
            .await
            .expect("client");

        let error = client
            .sql("SELECT 1")
            .await
            .err()
            .expect("every attempt resets");
        assert!(
            matches!(error, Error::ConnectionReset { .. }),
            "expected ConnectionReset, got {error:?}"
        );
    }

    #[tokio::test]
    async fn sql_with_params_reports_an_exhausted_reset_as_connection_reset() {
        let url = always_reset_endpoint().await;
        let client = SpiceClient::builder()
            .flight_url(&url)
            .build()
            .await
            .expect("client");

        let error = client
            .sql_with_params("SELECT 1", None)
            .await
            .err()
            .expect("every attempt resets");
        assert!(
            matches!(error, Error::ConnectionReset { .. }),
            "expected ConnectionReset, got {error:?}"
        );
    }

    /// A reset that outlived the retries is still a reset. Reporting it as a plain query
    /// failure loses the only signal that says the failure is worth retrying, and callers
    /// classify on the variant.
    #[test]
    fn exhausted_retry_keeps_a_reset_classified() {
        let error: GenericError = Box::new(tonic::Status::unknown("transport error"));
        assert!(matches!(
            exhausted_retry_error(error),
            Error::ConnectionReset { .. }
        ));
    }

    #[test]
    fn exhausted_retry_reports_other_failures_as_query_errors() {
        let error: GenericError = Box::new(tonic::Status::invalid_argument("no such table"));
        assert!(matches!(exhausted_retry_error(error), Error::Query { .. }));
    }

    /// The message has to survive, or the reset arrives with nothing to diagnose it by.
    #[test]
    fn exhausted_retry_keeps_the_reset_message() {
        let error: GenericError = Box::new(tonic::Status::unknown("transport error"));
        let Error::ConnectionReset { message } = exhausted_retry_error(error) else {
            panic!("expected a ConnectionReset");
        };
        assert!(message.contains("transport error"), "{message}");
    }

    fn test_client(http_base_url: Option<&str>) -> SpiceClient {
        let flight_channel = Endpoint::from_static("http://127.0.0.1:50051").connect_lazy();

        SpiceClient {
            flight: Arc::new(SqlFlightClient::new(
                flight_channel,
                None,
                None,
                None,
                MAX_RETRIES,
            )),
            http_client: http_base_url.map(|base_url| {
                let client = QueryHttpClient::new(base_url, None);
                Arc::new(client.expect("build client"))
            }),
        }
    }

    fn query_manifest_response(query_id: &str, status: QueryStatus) -> serde_json::Value {
        json!({
            "query_id": query_id,
            "status": status,
            "manifest": {
                "format": "json",
                "schema": {
                    "column_count": 1,
                    "columns": [
                        {
                            "name": "answer",
                            "type_name": "Int32",
                            "nullable": false,
                            "position": 0
                        }
                    ]
                },
                "total_row_count": 2,
                "total_chunk_count": 1
            }
        })
    }

    #[test]
    fn test_client_builder_default() {
        let builder = SpiceClientBuilder::default();
        assert!(builder.api_key.is_none());
        assert!(builder.user_agent.is_none());
        assert!(builder.flight_url.is_none());
        assert!(builder.http_url.is_none());
        assert!(builder.cache_control.is_none());
        assert_eq!(builder.max_retries, MAX_RETRIES);
    }

    #[test]
    fn test_client_builder_new() {
        let builder = SpiceClientBuilder::new();
        assert!(builder.api_key.is_none());
        assert!(builder.user_agent.is_none());
        assert!(builder.flight_url.is_none());
        assert!(builder.http_url.is_none());
        assert!(builder.cache_control.is_none());
        assert_eq!(builder.max_retries, MAX_RETRIES);
    }

    #[test]
    fn test_client_builder_api_key() {
        let builder = SpiceClientBuilder::new().api_key("test_key");
        assert_eq!(builder.api_key, Some("test_key".to_string()));
    }

    #[test]
    fn test_client_builder_user_agent() {
        let builder = SpiceClientBuilder::new().user_agent("custom-agent/1.0");
        assert_eq!(builder.user_agent, Some("custom-agent/1.0".to_string()));
    }

    #[test]
    fn test_client_builder_flight_url() {
        let builder = SpiceClientBuilder::new().flight_url("https://custom.endpoint.io");
        assert_eq!(
            builder.flight_url,
            Some("https://custom.endpoint.io".to_string())
        );
    }

    #[test]
    fn test_client_builder_http_url() {
        let builder = SpiceClientBuilder::new().http_url("http://localhost:8090");
        assert_eq!(builder.http_url, Some("http://localhost:8090".to_string()));
    }

    #[test]
    fn test_client_builder_max_retries() {
        let builder = SpiceClientBuilder::new().max_retries(10);
        assert_eq!(builder.max_retries, 10);
    }

    #[test]
    fn test_client_builder_cache_control() {
        let builder = SpiceClientBuilder::new().cache_control("no-cache");
        assert_eq!(builder.cache_control, Some("no-cache".to_string()));
    }

    #[test]
    fn test_client_builder_use_spiceai_cloud() {
        let builder = SpiceClientBuilder::new().use_spiceai_cloud();
        assert_eq!(
            builder.flight_url,
            Some(SPICE_CLOUD_FLIGHT_ADDR.to_string())
        );
    }

    #[test]
    fn test_client_builder_chaining() {
        let builder = SpiceClientBuilder::new()
            .api_key("my_api_key")
            .user_agent("my-agent/2.0")
            .max_retries(5)
            .cache_control("max-age=3600")
            .use_spiceai_cloud();

        assert_eq!(builder.api_key, Some("my_api_key".to_string()));
        assert_eq!(builder.user_agent, Some("my-agent/2.0".to_string()));
        assert_eq!(builder.max_retries, 5);
        assert_eq!(builder.cache_control, Some("max-age=3600".to_string()));
        assert_eq!(
            builder.flight_url,
            Some(SPICE_CLOUD_FLIGHT_ADDR.to_string())
        );
    }

    #[test]
    fn test_client_builder_flight_url_overrides_cloud() {
        let builder = SpiceClientBuilder::new()
            .use_spiceai_cloud()
            .flight_url("https://custom.endpoint.io");

        // flight_url should override the cloud endpoint
        assert_eq!(
            builder.flight_url,
            Some("https://custom.endpoint.io".to_string())
        );
    }

    #[test]
    fn test_client_builder_cloud_overrides_flight_url() {
        let builder = SpiceClientBuilder::new()
            .flight_url("https://custom.endpoint.io")
            .use_spiceai_cloud();

        // use_spiceai_cloud should override custom flight_url
        assert_eq!(
            builder.flight_url,
            Some(SPICE_CLOUD_FLIGHT_ADDR.to_string())
        );
    }

    #[test]
    fn test_spice_client_has_builder() {
        let builder = SpiceClient::builder();
        assert!(builder.api_key.is_none());
    }

    #[test]
    fn test_error_display_query() {
        let error = Error::Query {
            source: "test error".into(),
        };
        let display = format!("{error}");
        assert!(display.contains("Query execution failed"));
    }

    #[test]
    fn test_error_display_connection_reset() {
        let error = Error::ConnectionReset {
            message: "connection lost".to_string(),
        };
        let display = format!("{error}");
        assert!(display.contains("Connection reset"));
        assert!(display.contains("connection lost"));
    }

    // Edge case tests

    #[test]
    fn test_client_builder_empty_api_key() {
        let builder = SpiceClientBuilder::new().api_key("");
        assert_eq!(builder.api_key, Some(String::new()));
    }

    #[test]
    fn test_client_builder_empty_user_agent() {
        let builder = SpiceClientBuilder::new().user_agent("");
        assert_eq!(builder.user_agent, Some(String::new()));
    }

    #[test]
    fn test_client_builder_empty_flight_url() {
        let builder = SpiceClientBuilder::new().flight_url("");
        assert_eq!(builder.flight_url, Some(String::new()));
    }

    #[test]
    fn test_client_builder_zero_max_retries() {
        let builder = SpiceClientBuilder::new().max_retries(0);
        assert_eq!(builder.max_retries, 0);
    }

    #[test]
    fn test_client_builder_max_retries_u32_max() {
        let builder = SpiceClientBuilder::new().max_retries(u32::MAX);
        assert_eq!(builder.max_retries, u32::MAX);
    }

    #[test]
    fn test_client_builder_special_chars_in_api_key() {
        let api_key = "abc123!@#$%^&*()_+-=[]{}|;':\",./<>?";
        let builder = SpiceClientBuilder::new().api_key(api_key);
        assert_eq!(builder.api_key, Some(api_key.to_string()));
    }

    #[test]
    fn test_client_builder_unicode_user_agent() {
        let user_agent = "测试-agent/1.0 🚀";
        let builder = SpiceClientBuilder::new().user_agent(user_agent);
        assert_eq!(builder.user_agent, Some(user_agent.to_string()));
    }

    #[test]
    fn test_client_builder_multiple_calls_same_method() {
        let builder = SpiceClientBuilder::new()
            .api_key("first")
            .api_key("second")
            .api_key("third");
        assert_eq!(builder.api_key, Some("third".to_string()));
    }

    #[test]
    fn test_error_query_stream() {
        let error = Error::QueryStream {
            source: FlightError::NotYetImplemented("test".to_string()),
        };
        let display = format!("{error}");
        assert!(display.contains("Failed to process query stream"));
    }

    #[test]
    fn test_client_builder_cache_control_variations() {
        // Test various cache control header values
        let cases = ["no-cache", "max-age=0", "no-store", "private, max-age=3600"];

        for case in cases {
            let builder = SpiceClientBuilder::new().cache_control(case);
            assert_eq!(builder.cache_control, Some(case.to_string()));
        }
    }

    #[test]
    fn test_client_builder_whitespace_in_values() {
        let builder = SpiceClientBuilder::new()
            .api_key("  key with spaces  ")
            .user_agent("  agent  ");
        assert_eq!(builder.api_key, Some("  key with spaces  ".to_string()));
        assert_eq!(builder.user_agent, Some("  agent  ".to_string()));
    }

    #[tokio::test]
    async fn test_async_methods_require_http_url() {
        let client = test_client(None);

        assert!(matches!(
            client.query("SELECT 1").await,
            Err(QueryError::HttpError { .. })
        ));
        assert!(matches!(
            client.queries(None, None).await,
            Err(QueryError::HttpError { .. })
        ));
        assert!(matches!(
            client.get_query("qry_123"),
            Err(QueryError::HttpError { .. })
        ));
        assert!(matches!(
            client.cancel_query("qry_123").await,
            Err(QueryError::HttpError { .. })
        ));
        assert!(matches!(
            client.refresh_dataset("orders").await,
            Err(DatasetError::HttpError { .. })
        ));
        assert!(matches!(
            client
                .refresh_dataset_with_options(
                    "orders",
                    DatasetRefreshRequest::new().with_refresh_mode(DatasetRefreshMode::Append),
                )
                .await,
            Err(DatasetError::HttpError { .. })
        ));
    }

    #[tokio::test]
    async fn test_query_with_bindings_submits_parameters() {
        let server = MockServer::start().await;
        let query_id = "qry_params";
        let sql = "SELECT * FROM t WHERE status = $1 AND n > $2";

        Mock::given(method("POST"))
            .and(path("/v1/queries"))
            .and(body_json(
                json!({ "sql": sql, "parameters": ["active", 5] }),
            ))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "query_id": query_id,
                "status": "PENDING",
                "status_url": format!("{}/v1/queries/{query_id}/status", server.uri()),
                "results_url": format!("{}/v1/queries/{query_id}/results", server.uri())
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));
        let job = client
            .query_with_bindings(sql, QueryParameters::new().push("active").push(5_i64))
            .await
            .expect("submit parameterized async query");
        assert_eq!(job.id(), query_id);
    }

    #[tokio::test]
    async fn test_query_with_options_submits_all_fields() {
        let server = MockServer::start().await;
        let query_id = "qry_opts";
        let sql = "SELECT * FROM t WHERE status = $1";

        Mock::given(method("POST"))
            .and(path("/v1/queries"))
            .and(body_json(json!({
                "sql": sql,
                "parameters": ["active"],
                "timeout_seconds": 300,
                "maximum_size": 100_000_000
            })))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "query_id": query_id,
                "status": "PENDING",
                "status_url": format!("{}/v1/queries/{query_id}/status", server.uri()),
                "results_url": format!("{}/v1/queries/{query_id}/results", server.uri())
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));
        let job = client
            .query_with_options(
                sql,
                QuerySubmitOptions::new()
                    .bindings(QueryParameters::new().push("active"))
                    .timeout_seconds(300)
                    .maximum_size(100_000_000),
            )
            .await
            .expect("submit async query with options");
        assert_eq!(job.id(), query_id);
    }

    #[tokio::test]
    async fn test_query_with_bindings_rejects_unsupported_param() {
        // A binary bind value has no JSON scalar form; the client must reject it
        // locally, before any HTTP request is attempted.
        let client = test_client(Some("http://127.0.0.1:1"));
        let err = client
            .query_with_bindings("SELECT $1", QueryParameters::new().push(vec![1_u8, 2, 3]))
            .await
            .expect_err("binary bind parameter should be rejected before submit");
        assert!(matches!(err, QueryError::InvalidParameter { .. }));
    }

    #[tokio::test]
    async fn test_async_query_wrappers_and_job_lifecycle() {
        let server = MockServer::start().await;
        let query_id = "qry_123";

        Mock::given(method("POST"))
            .and(path("/v1/queries"))
            .and(body_json(json!({ "sql": "SELECT 1" })))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({
                "query_id": query_id,
                "status": "PENDING",
                "status_url": format!("{}/v1/queries/{query_id}/status", server.uri()),
                "results_url": format!("{}/v1/queries/{query_id}/results", server.uri())
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v1/queries"))
            .and(query_param("status", "running"))
            .and(query_param("limit", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queries": [
                    {
                        "query_id": query_id,
                        "status": "RUNNING",
                        "created_at": "2025-01-01T00:00:00Z",
                        "sql_preview": "SELECT 1"
                    }
                ],
                "total_count": 1
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/queries/{query_id}/status")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "RUNNING"
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/queries/{query_id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(query_manifest_response(query_id, QueryStatus::Succeeded)),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(format!("/v1/queries/{query_id}/results/chunks/0")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "chunk_index": 0,
                "row_offset": 0,
                "row_count": 2,
                "data_array": [
                    { "answer": 1 },
                    { "answer": 2 }
                ]
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path(format!("/v1/queries/{query_id}/cancel")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "query_id": query_id,
                "status": "CANCELLED"
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let queries = client
            .queries(Some("running"), Some(5))
            .await
            .expect("list async queries");
        assert_eq!(queries.total_count, Some(1));
        assert_eq!(queries.queries.len(), 1);
        assert_eq!(queries.queries[0].query_id, query_id);
        assert_eq!(queries.queries[0].status, QueryStatus::Running);

        let job = client.query("SELECT 1").await.expect("submit async query");
        assert_eq!(job.id(), query_id);
        assert_eq!(
            job.status().await.expect("get query status"),
            QueryStatus::Running
        );

        let info = job.info().await.expect("get query info");
        assert_eq!(info.query_id, query_id);
        assert_eq!(info.status, QueryStatus::Succeeded);
        let info_result = info.result.expect("query result metadata");
        assert_eq!(info_result.total_rows, 2);
        assert_eq!(info_result.total_chunks, 1);

        let wait_result = job.wait().await.expect("wait for query completion");
        assert_eq!(wait_result.total_rows, 2);
        assert_eq!(wait_result.total_chunks, 1);

        let wait_timeout_result = client
            .get_query(query_id)
            .expect("resume query handle")
            .with_poll_interval(Duration::from_millis(1))
            .wait_timeout(Duration::from_millis(10))
            .await
            .expect("wait for query completion with timeout");
        assert_eq!(wait_timeout_result.total_rows, 2);

        let batches = client
            .get_query(query_id)
            .expect("resume query handle")
            .results()
            .await
            .expect("fetch query results");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 2);
        let answers = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int32Array>()
            .expect("downcast answer column to Int32Array");
        assert_eq!(answers.value(0), 1);
        assert_eq!(answers.value(1), 2);

        let streamed_batches = client
            .get_query(query_id)
            .expect("resume query handle")
            .results_stream()
            .await
            .expect("stream query results")
            .try_collect::<Vec<_>>()
            .await
            .expect("collect streamed query results");
        assert_eq!(streamed_batches.len(), 1);
        assert_eq!(streamed_batches[0].num_rows(), 2);

        let cancelled = job.cancel().await.expect("cancel query from job handle");
        assert_eq!(cancelled.query_id, query_id);
        assert_eq!(cancelled.status, QueryStatus::Cancelled);

        let cancelled_via_client = client
            .cancel_query(query_id)
            .await
            .expect("cancel query from client wrapper");
        assert_eq!(cancelled_via_client.query_id, query_id);
        assert_eq!(cancelled_via_client.status, QueryStatus::Cancelled);
    }

    #[tokio::test]
    async fn test_query_job_wait_timeout_returns_timeout_for_running_query() {
        let server = MockServer::start().await;
        let query_id = "qry_timeout";

        Mock::given(method("GET"))
            .and(path(format!("/v1/queries/{query_id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "query_id": query_id,
                "status": "RUNNING"
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));
        let err = client
            .get_query(query_id)
            .expect("create query handle")
            .with_poll_interval(Duration::from_millis(1))
            .wait_timeout(Duration::from_millis(5))
            .await
            .expect_err("query wait should time out while still running");

        assert!(matches!(err, QueryError::Timeout { .. }));
    }

    #[tokio::test]
    async fn test_refresh_dataset_wrappers() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/datasets/orders_default/acceleration/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": "Refresh started"
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/datasets/orders_custom/acceleration/refresh"))
            .and(body_json(json!({
                "refresh_sql": "SELECT * FROM orders",
                "refresh_mode": "append",
                "refresh_jitter_max": "30s"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "message": "Refresh scheduled"
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let default_refresh = client
            .refresh_dataset("orders_default")
            .await
            .expect("refresh dataset with default settings");
        assert_eq!(default_refresh.message, "Refresh started");

        let custom_refresh = client
            .refresh_dataset_with_options(
                "orders_custom",
                DatasetRefreshRequest::new()
                    .with_refresh_sql("SELECT * FROM orders")
                    .with_refresh_mode(DatasetRefreshMode::Append)
                    .with_refresh_jitter_max("30s"),
            )
            .await
            .expect("refresh dataset with explicit overrides");
        assert_eq!(custom_refresh.message, "Refresh scheduled");
    }

    #[tokio::test]
    async fn test_search_posts_request_and_parses_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .and(body_json(json!({
                "text": "tokyo plane tickets",
                "datasets": ["app_messages"],
                "limit": 3,
                "additional_columns": ["timestamp"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "matches": {"message": ["I booked us some tickets", "direct to Narita"]},
                        "dataset": "app_messages",
                        "primary_key": {"id": "6fd5a215"},
                        "data": {"timestamp": 1_724_716_542_i64},
                        "_score": 0.914_321
                    },
                    {
                        "matches": {"message": ["we're sitting together"]},
                        "dataset": "app_messages",
                        "_score": 0.787_654
                    }
                ],
                "duration_ms": 42
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let response = client
            .search(
                SearchRequest::new("tokyo plane tickets")
                    .with_datasets(["app_messages"])
                    .with_limit(3)
                    .with_additional_columns(["timestamp"]),
            )
            .await
            .expect("search succeeds");

        assert_eq!(response.duration_ms, 42);
        assert_eq!(response.len(), 2);

        let first = &response.results[0];
        assert_eq!(first.dataset, "app_messages");
        // One column can contribute several chunks to a single match.
        assert_eq!(first.matches["message"].len(), 2);
        assert_eq!(first.data["timestamp"], 1_724_716_542_i64);

        // The runtime omits data and primary_key when they are empty.
        assert!(response.results[1].data.is_empty());
        assert!(response.results[1].primary_key.is_empty());
    }

    #[tokio::test]
    async fn test_search_surfaces_runtime_error_body() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/search"))
            .respond_with(ResponseTemplate::new(400).set_body_string("No data sources provided"))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let err = client
            .search(SearchRequest::new("tokyo"))
            .await
            .expect_err("runtime rejects the search");

        let message = err.to_string();
        assert!(message.contains("No data sources provided"), "{message}");
        assert!(message.contains("400"), "{message}");
    }

    #[tokio::test]
    async fn test_search_validates_before_sending() {
        let server = MockServer::start().await;
        // No mock is mounted: a request reaching the server fails the test.
        let client = test_client(Some(&server.uri()));

        let err = client
            .search(SearchRequest::new(""))
            .await
            .expect_err("empty text is rejected");
        assert!(err.to_string().contains("non-empty"), "{err}");

        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_search_requires_http_url() {
        let client = test_client(None);

        let err = client
            .search(SearchRequest::new("tokyo"))
            .await
            .expect_err("search without an HTTP endpoint fails");
        assert!(err.to_string().contains("http_url"), "{err}");
    }

    #[tokio::test]
    async fn test_nsql_posts_request_and_parses_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/nsql"))
            // Without this media type the runtime answers with a bare array of
            // rows and the generated SQL is lost, so pin it.
            .and(header("accept", "application/vnd.spiceai.nsql.v1+json"))
            .and(body_json(json!({
                "query": "top 5 customers by revenue",
                "datasets": ["sales"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "row_count": 2,
                "schema": {
                    "fields": [
                        {"name": "customer_id", "data_type": "Utf8", "nullable": false},
                        {"name": "total", "data_type": "Int64", "nullable": false}
                    ]
                },
                "data": [
                    {"customer_id": "12345", "total": 150_000},
                    {"customer_id": "67890", "total": 125_000}
                ],
                "sql": "SELECT customer_id, sum(total) AS total FROM sales GROUP BY customer_id"
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let response = client
            .nsql(NsqlRequest::new("top 5 customers by revenue").with_datasets(["sales"]))
            .await
            .expect("nsql succeeds");

        assert_eq!(
            response.sql,
            "SELECT customer_id, sum(total) AS total FROM sales GROUP BY customer_id"
        );
        assert_eq!(response.row_count, 2);
        assert_eq!(response.len(), 2);
        assert_eq!(response.data[0]["customer_id"], "12345");
        assert_eq!(response.schema.fields.len(), 2);
        assert_eq!(response.schema.fields[0].data_type, "Utf8");
    }

    #[tokio::test]
    async fn test_nsql_generate_sql_requests_the_sql_media_type() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/nsql"))
            .and(header("accept", "application/sql"))
            .respond_with(
                // This media type answers with the bare query text.
                ResponseTemplate::new(200).set_body_string("\n  SELECT count(*) FROM orders\n"),
            )
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let sql = client
            .nsql_generate_sql(NsqlRequest::new("how many orders"))
            .await
            .expect("nsql_generate_sql succeeds");

        assert_eq!(sql, "SELECT count(*) FROM orders");
    }

    #[tokio::test]
    async fn test_nsql_context_requests_markdown_with_query_params() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/nsql/context"))
            .and(header("accept", "text/markdown"))
            .and(query_param("datasets", "sales.orders"))
            .and(query_param("include_sampling", "true"))
            .and(query_param("sampling_limit", "5"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("# Spice.ai NSQL Context\n\n## Datasets\n- `sales.orders`\n"),
            )
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let context = client
            .nsql_context(
                NsqlContextRequest::new()
                    .with_datasets(["sales.orders"])
                    .with_sampling(true)
                    .with_sampling_limit(5),
            )
            .await
            .expect("nsql_context succeeds");

        assert!(context.contains("## Datasets"), "{context}");
        assert!(context.contains("sales.orders"), "{context}");
    }

    #[tokio::test]
    async fn test_nsql_context_omits_unset_options() {
        let server = MockServer::start().await;

        // A default request carries no query string at all, so the runtime
        // applies its own defaults rather than the SDK guessing them.
        Mock::given(method("GET"))
            .and(path("/v1/nsql/context"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Spice.ai NSQL Context"))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let request = NsqlContextRequest::new();
        assert!(request.query_pairs().is_empty());

        client
            .nsql_context(request)
            .await
            .expect("nsql_context succeeds");
    }

    #[tokio::test]
    async fn test_nsql_context_validates_limits_before_sending() {
        let server = MockServer::start().await;
        // No mock is mounted: a request reaching the server fails the test.
        let client = test_client(Some(&server.uri()));

        let err = client
            .nsql_context(NsqlContextRequest::new().with_sampling_limit(101))
            .await
            .expect_err("a limit above the runtime maximum is rejected");

        let message = err.to_string();
        assert!(message.contains("sampling_limit"), "{message}");
        assert!(message.contains("100"), "{message}");
    }

    #[tokio::test]
    async fn test_nsql_context_surfaces_runtime_error_body() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/nsql/context"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("Dataset 'sales.orders' not found"),
            )
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let err = client
            .nsql_context(NsqlContextRequest::new().with_datasets(["sales.orders"]))
            .await
            .expect_err("runtime rejects the request");

        let message = err.to_string();
        assert!(message.contains("not found"), "{message}");
        assert!(message.contains("400"), "{message}");
    }

    #[tokio::test]
    async fn test_nsql_surfaces_runtime_error_body() {
        let server = MockServer::start().await;

        // A missing or ambiguous model is the most common NSQL failure and the
        // runtime explains it in the body.
        Mock::given(method("POST"))
            .and(path("/v1/nsql"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(
                    "No model specified and no compatible LLM model is configured.",
                ),
            )
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        let err = client
            .nsql(NsqlRequest::new("how many orders"))
            .await
            .expect_err("runtime rejects the request");

        let message = err.to_string();
        assert!(message.contains("No model specified"), "{message}");
        assert!(message.contains("400"), "{message}");
    }

    #[tokio::test]
    async fn test_nsql_validates_before_sending() {
        let server = MockServer::start().await;
        // No mock is mounted: a request reaching the server fails the test.
        let client = test_client(Some(&server.uri()));

        let err = client
            .nsql(NsqlRequest::new("   "))
            .await
            .expect_err("empty query is rejected");
        assert!(err.to_string().contains("non-empty"), "{err}");

        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_nsql_requires_http_url() {
        let client = test_client(None);

        let err = client
            .nsql(NsqlRequest::new("how many orders"))
            .await
            .expect_err("nsql without an HTTP endpoint fails");
        assert!(err.to_string().contains("http_url"), "{err}");
    }

    #[tokio::test]
    async fn test_active_queries_lists_from_the_runtime() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/sql/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "queries": [
                    {
                        "query_id": "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70",
                        "protocol": "flightsql",
                        "sql_preview": "SELECT * FROM taxi_trips",
                        "started_at_ms": 1_750_000_000_000_u64
                    }
                ],
                "total_count": 1
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));
        let active = client
            .active_queries()
            .await
            .expect("list the active queries");

        assert_eq!(active.total_count, 1);
        assert_eq!(
            active.queries[0].query_id,
            "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70"
        );
        assert_eq!(active.queries[0].protocol, "flightsql");
    }

    #[tokio::test]
    async fn test_active_queries_maps_a_forbidden_response() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/sql/active"))
            .respond_with(ResponseTemplate::new(403).set_body_string("write access required"))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));
        let err = client
            .active_queries()
            .await
            .expect_err("a 403 should not list queries");

        assert!(matches!(err, ActiveQueryError::WriteAccessRequired));
    }

    #[tokio::test]
    async fn test_active_queries_reports_an_unconfigured_http_endpoint() {
        let client = test_client(None);
        let err = client
            .active_queries()
            .await
            .expect_err("listing needs an HTTP endpoint");

        match err {
            ActiveQueryError::HttpError { message } => {
                assert!(message.contains("http_url()"), "unexpected: {message}");
            }
            other => panic!("expected an HttpError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cancel_active_query_posts_to_the_cancel_route() {
        let server = MockServer::start().await;
        let query_id = "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70";

        Mock::given(method("POST"))
            .and(path(format!("/v1/sql/{query_id}/cancel")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "query_id": query_id,
                "status": "cancelled"
            })))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));
        let cancelled = client
            .cancel_active_query(query_id)
            .await
            .expect("cancel the active query");

        assert_eq!(cancelled.query_id, query_id);
        assert_eq!(cancelled.status, "cancelled");
    }

    #[tokio::test]
    async fn test_cancel_active_query_maps_each_error_status() {
        let server = MockServer::start().await;

        // Every id here is a well-formed UUID, so each reaches the runtime and
        // exercises the status mapping rather than the local shape check.
        const FORBIDDEN: &str = "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f71";
        const MISSING: &str = "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f72";
        const REJECTED: &str = "0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f73";

        for (id, status) in [(REJECTED, 400), (FORBIDDEN, 403), (MISSING, 404)] {
            Mock::given(method("POST"))
                .and(path(format!("/v1/sql/{id}/cancel")))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
        }

        let client = test_client(Some(&server.uri()));

        assert!(matches!(
            client
                .cancel_active_query(REJECTED)
                .await
                .expect_err("400 maps to InvalidQueryId"),
            ActiveQueryError::InvalidQueryId { query_id } if query_id == REJECTED
        ));
        assert!(matches!(
            client
                .cancel_active_query(FORBIDDEN)
                .await
                .expect_err("403 maps to WriteAccessRequired"),
            ActiveQueryError::WriteAccessRequired
        ));
        assert!(matches!(
            client
                .cancel_active_query(MISSING)
                .await
                .expect_err("404 maps to NotFound"),
            ActiveQueryError::NotFound { query_id } if query_id == MISSING
        ));
    }

    #[tokio::test]
    async fn test_cancel_active_query_rejects_an_id_that_could_reroute_the_request() {
        let server = MockServer::start().await;

        // Any request at all is a failure here: `.` and `..` survive
        // percent-encoding and the URL parser then resolves them away, so an id
        // like `..` would turn this POST into one at `/v1/sql/cancel`.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "query_id": "rerouted",
                "status": "cancelled"
            })))
            .expect(0)
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        for id in ["..", ".", "../queries/escape", "not-a-uuid", ""] {
            let err = client
                .cancel_active_query(id)
                .await
                .expect_err("an id that is not a UUID cannot cancel a query");

            assert!(
                matches!(err, ActiveQueryError::InvalidQueryId { ref query_id } if query_id == id),
                "id {id:?} produced {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_async_query_id_cannot_reroute_a_request_onto_another_endpoint() {
        let server = MockServer::start().await;

        // The routes a query id must never be able to reach. `..` is resolved
        // away by the URL parser and `#` truncates the path at the fragment, so
        // an id formatted into a path string chooses the route — and the client
        // attaches its API key to whatever it then calls.
        for route in [
            "/v1/datasets/orders/acceleration/refresh",
            "/v1/sql/0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70/cancel",
            "/v1/cancel",
            "/v1/queries",
        ] {
            Mock::given(path(route))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"message": "rerouted"})),
                )
                .expect(0)
                .mount(&server)
                .await;
        }

        // Anything that does reach the intended route is the runtime's to
        // refuse, and it answers 404 for an id no job carries.
        Mock::given(path_regex(r"^/v1/queries/[^/]+(/.*)?$"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = test_client(Some(&server.uri()));

        for id in [
            "..",
            ".",
            "../datasets/orders/acceleration/refresh#",
            "../sql/0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70",
        ] {
            let job = client.get_query(id).expect("create query handle");

            assert!(job.cancel().await.is_err(), "cancel accepted the id {id:?}");
            assert!(job.status().await.is_err(), "status accepted the id {id:?}");
            assert!(
                job.results().await.is_err(),
                "results accepted the id {id:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_cancel_active_query_reports_an_unconfigured_http_endpoint() {
        let client = test_client(None);
        let err = client
            .cancel_active_query("0198f0a1-9c3d-7c4e-8a11-2b3c4d5e6f70")
            .await
            .expect_err("cancellation needs an HTTP endpoint");

        match err {
            ActiveQueryError::HttpError { message } => {
                assert!(message.contains("http_url()"), "unexpected: {message}");
            }
            other => panic!("expected an HttpError, got {other:?}"),
        }
    }
}
