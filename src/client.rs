use crate::{
    config::{GenericError, SPICE_CLOUD_FLIGHT_ADDR, SPICE_LOCAL_FLIGHT_ADDR},
    flight::SqlFlightClient,
    tls::new_tls_flight_channel,
};
use arrow::record_batch::RecordBatch;
use arrow_flight::decode::FlightRecordBatchStream;
use tonic::transport::Channel;

const MAX_RETRIES: u32 = 5;

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

/// The `SpiceClient` is the main entry point for interacting with the Spice API.
/// It provides methods for querying the Spice Flight endpoint.
#[allow(clippy::module_name_repetitions)]
pub struct SpiceClient {
    flight: SqlFlightClient,
}

impl SpiceClient {
    /// Creates a new `SpiceClient` with the given API key and default user agent.
    /// ```
    /// use spiceai::Client;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut client = Client::new("API_KEY").await.unwrap();
    /// }
    /// ```
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` for any query error
    pub async fn new(api_key: &str) -> Result<Self, GenericError> {
        let config = SpiceClientConfig::load_from_default().await?;

        Ok(Self {
            flight: SqlFlightClient::new(
                config.flight_channel,
                Some(api_key.to_string()),
                None,
                None,
            ),
        })
    }

    #[must_use]
    pub fn builder() -> SpiceClientBuilder {
        SpiceClientBuilder::new()
    }

    /// Queries the Spice Flight endpoint with the given SQL query.
    /// ```
    /// # use spiceai::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #  let mut client = Client::new("API_KEY").await.unwrap();
    /// let data = client.query("SELECT * FROM taxi_trips LIMIT 10;").await;
    /// # }
    /// ````
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` for any query error
    pub async fn query(&mut self, query: &str) -> Result<FlightRecordBatchStream, GenericError> {
        let mut retry_count = 0;

        loop {
            match self.flight.query(query).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    let error_str = e.to_string();

                    if retry_count < MAX_RETRIES && is_retryable_error(&error_str) {
                        retry_count += 1;
                        eprintln!(
                            "Connection error on query attempt {retry_count}/{MAX_RETRIES}: {e}. Retrying..."
                        );

                        // Exponential backoff
                        tokio::time::sleep(std::time::Duration::from_millis(
                            100 * (1 << retry_count),
                        ))
                        .await;

                        continue;
                    }

                    return Err(e);
                }
            }
        }
    }

    /// Optional parameterized query with the Spice Flight endpoint with the given SQL query.
    /// /// If `params` is `None`, it behaves like a regular query.
    /// `params` is a parameter binding `RecordBatch`.
    /// <https://docs.rs/arrow-flight/latest/arrow_flight/sql/client/struct.PreparedStatement.html#method.set_parameters>
    /// ```
    /// # use spiceai::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #  let mut client = Client::new("API_KEY").await.unwrap();
    /// let data = client.query_with_params("SELECT * FROM taxi_trips LIMIT 10;", None).await;
    /// # }
    /// ````
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` for any query error
    pub async fn query_with_params(
        &mut self,
        query: &str,
        params: Option<RecordBatch>,
    ) -> Result<FlightRecordBatchStream, GenericError> {
        let mut retry_count = 0;

        loop {
            match self.flight.query_with_params(query, params.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    let error_str = e.to_string();

                    if retry_count < MAX_RETRIES && is_retryable_error(&error_str) {
                        retry_count += 1;
                        eprintln!(
                            "Connection error on query attempt {retry_count}/{MAX_RETRIES}: {e}. Retrying..."
                        );

                        // Exponential backoff
                        tokio::time::sleep(std::time::Duration::from_millis(
                            100 * (1 << retry_count),
                        ))
                        .await;

                        continue;
                    }

                    return Err(e);
                }
            }
        }
    }
}

fn is_retryable_error(error_str: &str) -> bool {
    error_str
        .to_lowercase()
        .contains("connection is reset by the server. please retry the request.")
}

/// Builder for creating a `SpiceClient`.
///
/// By default the `SpiceClient` will use local spice runtime flight endpoint.
/// Follow [spiceai quickstart](https://github.com/spiceai/spiceai?tab=readme-ov-file#%EF%B8%8F-quickstart-local-machine) to setup local spice runtime.
/// ```
/// # use spiceai::ClientBuilder;
/// #
/// # #[tokio::main]
/// # async fn main() {
/// #    let mut client = ClientBuilder::new()
/// #      .build()
/// #      .await
/// #      .unwrap();
/// # }
/// ```
/// To use default Spice.ai Cloud endpoints, you can use the `with_spiceai_cloud()` method.
///
/// ```
/// # use spiceai::ClientBuilder;
/// #
/// # #[tokio::main]
/// # async fn main() {
/// #    let mut client = ClientBuilder::new()
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
    cache_control: Option<String>,
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
            cache_control: None,
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

    /// Configures the cache control to use the given Spice Flight endpoint.
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

    /// Builds the `SpiceClient` with the specified configuration.
    ///
    /// ## Errors
    ///
    /// - `Box<dyn Error + Send + Sync>` if flight channel creation fails
    pub async fn build(self) -> Result<SpiceClient, GenericError> {
        let flight_channel = match self.flight_url {
            Some(url) => new_tls_flight_channel(&url).await?,
            None => new_tls_flight_channel(SPICE_LOCAL_FLIGHT_ADDR).await?,
        };

        Ok(SpiceClient {
            flight: SqlFlightClient::new(
                flight_channel,
                self.api_key.clone(),
                self.user_agent.clone(),
                self.cache_control.clone(),
            ),
        })
    }
}
