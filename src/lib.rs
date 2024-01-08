//!# Rust Spice SDK
//! Rust SDK for [Spice.ai](https://spice.ai).
//! ## Installation
//! Add the following to your `Cargo.toml`:
//! ```toml
//! [dependencies]
//! spice-rs = { git = "https://github.com/spiceai/spice-rs", tag = "v1.0.2" }
//! ```
//! ## Usage
//! #### Arrow Query
//! ```rust
//! use spice_rs::Client;
//!
//! #[tokio::main]
//! async fn main() {
//!   let mut client = Client::new("API_KEY").await.unwrap();
//!   let data = client.query("SELECT * FROM eth.recent_blocks LIMIT 10;").await;
//! }
//! ```
//! #### Firecache Query
//! ```rust
//! use spice_rs::Client;
//!
//! #[tokio::main]
//! async fn main() {
//!   let mut client = Client::new("API_KEY").await.unwrap();
//!   let data = client.fire_query("SELECT * FROM eth.recent_blocks LIMIT 10;").await;
//! }
//! ```
//! #### Prices
//! Get the supported pairs:
//! ```rust
//! use spice_rs::Client;
//!
//! #[tokio::main]
//! async fn main() {
//!   let mut client = Client::new("API_KEY").await.unwrap();
//!   let supported_pairs = client.get_supported_pairs().await;
//! }
//! ```
//! Get the latest price for a token pair:
//! ```rust
//! use spice_rs::Client;
//!
//! #[tokio::main]
//! async fn main() {
//!   let mut client = Client::new("API_KEY").await.unwrap();
//!   let price_data = client.get_prices(&["BTC-USDC"]).await;
//! }
//! ```
//! Get historical data:
//! ```rust
//! use spice_rs::Client;
//! use chrono::Utc;
//! use chrono::Duration;
//! use std::ops::Sub;
//!
//! #[tokio::main]
//! async fn main() {
//!   let mut client = Client::new("API_KEY").await.unwrap();
//!   let now = Utc::now();
//!   let start = now.sub(Duration::seconds(3600));
//!   let historical_price_data = client
//!     .get_historical_prices(&["BTC-USDC"], Some(start), Some(now), Option::None).await;
//! }
//! ```
//! ## Documentation
//! Check out our [Documentation](https://docs.spice.ai/sdks/rust-sdk) to learn more about how to use the Rust SDK.

mod client;
mod config;
mod flight;
mod prices;
mod tls;

pub use client::SpiceClient as Client;
pub use prices::{HistoricalPriceData, LatestPriceDetail, LatestPricesResponse};

// Further public exports and integrations
pub use futures::StreamExt;
