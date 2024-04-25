use crate::{
    config::{SPICE_CLOUD_FIRECACHE_ADDR, SPICE_CLOUD_FLIGHT_ADDR, SPICE_LOCAL_FLIGHT_ADDR},
    flight::SqlFlightClient,
    tls::new_tls_flight_channel,
};
use arrow_flight::decode::FlightRecordBatchStream;
use futures::try_join;
use std::error::Error;
use tonic::transport::Channel;

struct SpiceClientConfig {
    flight_channel: Channel,
    firecache_channel: Channel,
}

impl SpiceClientConfig {
    fn new(flight_channel: Channel, firecache_channel: Channel) -> Self {
        SpiceClientConfig {
            flight_channel,
            firecache_channel,
        }
    }

    pub async fn load_from_default() -> Result<SpiceClientConfig, Box<dyn Error>> {
        let (flight_chan, firecache_chan) = try_join!(
            new_tls_flight_channel(SPICE_CLOUD_FLIGHT_ADDR),
            new_tls_flight_channel(SPICE_CLOUD_FIRECACHE_ADDR)
        )?;

        Ok(SpiceClientConfig::new(flight_chan, firecache_chan))
    }
}

/// The `SpiceClient` is the main entry point for interacting with the Spice API.
/// It provides methods for querying the Spice Flight and Firecache endpoints.
#[allow(clippy::module_name_repetitions)]
pub struct SpiceClient {
    flight: SqlFlightClient,
    firecache: SqlFlightClient,
}

impl SpiceClient {
    /// Creates a new `SpiceClient` with the given API key.
    /// ```
    /// use spiceai::Client;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut client = Client::new("API_KEY").await.unwrap();
    /// }
    /// ```
    #[deprecated(note = "Use spiceai::ClientBuilder instead")]
    pub async fn new(api_key: &str) -> Result<Self, Box<dyn Error>> {
        let config = SpiceClientConfig::load_from_default().await?;

        Ok(Self {
            flight: SqlFlightClient::new(config.flight_channel, Some(api_key.to_string())),
            firecache: SqlFlightClient::new(config.firecache_channel, Some(api_key.to_string())),
        })
    }

    /// Queries the Spice Flight endpoint with the given SQL query.
    /// ```
    /// # use spiceai::Client;
    /// #
    /// # #[tokio::main]
    /// # async fn main() {
    /// #  let mut client = Client::new("API_KEY").await.unwrap();
    /// let data = client.query("SELECT * FROM eth.recent_blocks LIMIT 10;").await;
    /// # }
    /// ````
    pub async fn query(&mut self, query: &str) -> Result<FlightRecordBatchStream, Box<dyn Error>> {
        self.flight.query(query).await
    }

    /// Queries the Spice Firecache endpoint with the given SQL query.
    /// ```
    /// # use spiceai::Client;
    /// #
    /// #  #[tokio::main]
    /// # async fn main() {
    /// #  let mut client = Client::new("API_KEY").await.unwrap();
    /// let data = client.fire_query("SELECT * FROM eth.recent_blocks LIMIT 10;").await;
    /// # }
    /// ````
    pub async fn fire_query(
        &mut self,
        query: &str,
    ) -> Result<FlightRecordBatchStream, Box<dyn Error>> {
        self.firecache.query(query).await
    }
}

pub struct SpiceClientBuilder {
    api_key: Option<String>,
    // http_url: Option<String>,
    firecache_url: Option<String>,
    flight_url: Option<String>,
}

impl Default for SpiceClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A builder for creating a `SpiceClient`.
impl SpiceClientBuilder {
    pub fn new() -> Self {
        Self {
            api_key: None,
            // http_url: None,
            firecache_url: None,
            flight_url: None,
        }
    }

    pub fn with_api_key(mut self, api_key: &str) -> Self {
        self.api_key = Some(api_key.to_string());
        self
    }

    pub fn with_firecache_url(mut self, firecache_url: &str) -> Self {
        self.firecache_url = Some(firecache_url.to_string());
        self
    }

    pub fn with_flight_url(mut self, flight_url: &str) -> Self {
        self.flight_url = Some(flight_url.to_string());
        self
    }

    pub fn with_spiceai_cloud(mut self) -> Self {
        self.flight_url = Some(SPICE_CLOUD_FLIGHT_ADDR.to_string());
        self.firecache_url = Some(SPICE_CLOUD_FIRECACHE_ADDR.to_string());
        self
    }

    pub async fn build(self) -> Result<SpiceClient, Box<dyn Error>> {
        let flight_channel = match self.flight_url {
            Some(url) => new_tls_flight_channel(&url).await?,
            None => new_tls_flight_channel(SPICE_LOCAL_FLIGHT_ADDR).await?,
        };

        let firecache_channel = match self.firecache_url {
            Some(url) => new_tls_flight_channel(&url).await?,
            None => new_tls_flight_channel(SPICE_CLOUD_FIRECACHE_ADDR).await?,
        };

        Ok(SpiceClient {
            flight: SqlFlightClient::new(flight_channel, self.api_key.clone()),
            firecache: SqlFlightClient::new(firecache_channel, self.api_key.clone()),
        })
    }
}
