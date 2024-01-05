use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use chrono::{DateTime, Utc};
use reqwest::{Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};

use crate::config::HTTPS_ADDR;

#[derive(Debug, Deserialize)]
pub struct HistoricalPriceData {
    pub timestamp: DateTime<Utc>,
    pub price: f64,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub open: Option<f64>,
    pub close: Option<f64>,
}

impl fmt::Display for HistoricalPriceData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Timestamp: {}, Price: {}, High: {:?}, Low: {:?}, Open: {:?}, Close: {:?}",
            self.timestamp, self.price, self.high, self.low, self.open, self.close
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct LatestPriceDetail {
    #[serde(deserialize_with = "string_to_float_map")]
    pub prices: HashMap<String, f64>,
    #[serde(
        rename = "minPrice",
        default,
        deserialize_with = "string_to_float_option"
    )]
    pub min_price: Option<f64>,
    #[serde(
        rename = "maxPrice",
        default,
        deserialize_with = "string_to_float_option"
    )]
    pub max_price: Option<f64>,
    #[serde(
        rename = "meanPrice",
        default,
        deserialize_with = "string_to_float_option"
    )]
    pub mean_price: Option<f64>,
}
impl fmt::Display for LatestPriceDetail {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let prices_str = self
            .prices
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "Prices: [{}], Min Price: {:?}, Max Price: {:?}, Mean Price: {:?}",
            prices_str, self.min_price, self.max_price, self.mean_price
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct LatestPricesResponse {
    // This assumes each key in the JSON (like "BTC-USDC", "LTC-USDT") is dynamic and represents a currency pair
    #[serde(flatten)]
    pub prices: HashMap<String, LatestPriceDetail>,
}

impl fmt::Display for LatestPricesResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let prices_str = self
            .prices
            .iter()
            .map(|(pair, detail)| format!("Pair: {}, Details: [{}]", pair, detail))
            .collect::<Vec<_>>()
            .join("\n");
        write!(f, "{}", prices_str)
    }
}

fn string_to_float_map<'de, D>(deserializer: D) -> Result<HashMap<String, f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let map = HashMap::<String, String>::deserialize(deserializer)?;
    map.into_iter()
        .map(|(k, v)| v.parse::<f64>().map(|v_f64| (k, v_f64)))
        .collect::<Result<HashMap<String, f64>, std::num::ParseFloatError>>()
        .map_err(serde::de::Error::custom)
}

fn string_to_float_option<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: Option<String> = Option::deserialize(deserializer)?;
    match s {
        Some(str_val) => str_val
            .parse::<f64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

async fn map_reqwest_response<T: DeserializeOwned>(resp: Response) -> Result<T, Box<dyn Error>> {
    match resp.status() {
        StatusCode::OK => {
            let response: T = resp.json().await?;
            Ok(response)
        }
        StatusCode::BAD_REQUEST => Err("Bad request".into()),
        StatusCode::TOO_MANY_REQUESTS => Err("Rate limit exceeded, slow down".into()),
        StatusCode::INTERNAL_SERVER_ERROR => Err("Internal server error".into()),
        _ => Err(format!("Unexpected response status: {}", resp.status()).into()),
    }
}

pub struct PricesClient {
    base_url: String,
    _api_key: String,
    client: reqwest::Client,
}

impl PricesClient {
    pub fn new(base_url: Option<String>, api_key: String) -> Self {
        let default_url = HTTPS_ADDR.to_string();
        let client = reqwest::Client::new();
        PricesClient {
            base_url: base_url.unwrap_or(default_url),
            _api_key: api_key,
            client,
        }
    }

    fn add_headers(&self, request_builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request_builder
            .header("X-API-Key", &self._api_key)
            .header("Accept", "application/json")
            .header("User-Agent", "spice-rs 1.0")
    }

    pub async fn get_supported_pairs(&self) -> Result<Vec<String>, Box<dyn Error>> {
        let url = format!("{}/v1/prices/pairs", self.base_url);
        let request = self.client.get(&url);
        let response: Vec<String> = self.add_headers(request).send().await?.json().await?;
        Ok(response)
    }

    pub async fn get_prices(&self, pairs: &[&str]) -> Result<LatestPricesResponse, Box<dyn Error>> {
        let url = format!("{}/v1/prices?pairs={}", self.base_url, pairs.join(","));

        self.add_headers(self.client.get(&url))
            .send()
            .await
            .map(map_reqwest_response)?
            .await
    }

    pub async fn get_historical_prices(
        &self,
        pairs: &[&str],
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        granularity: Option<&str>,
    ) -> Result<HashMap<String, Vec<HistoricalPriceData>>, Box<dyn Error>> {
        let mut url = format!(
            "{}/v1/prices/historical?pairs={}",
            self.base_url,
            pairs.join(",")
        );

        if let Some(start_time) = start {
            let timestamp = start_time.timestamp();
            url.push_str(&format!("&start={}", timestamp));
        }

        if let Some(end_time) = end {
            let timestamp = end_time.timestamp();
            url.push_str(&format!("&end={}", timestamp));
        }

        if let Some(gran) = granularity {
            url.push_str(&format!("&granularity={}", gran));
        }

        println!("URL: {}", url);
        self.add_headers(self.client.get(&url))
            .send()
            .await
            .map(map_reqwest_response)?
            .await
    }
}
