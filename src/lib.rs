#![doc = include_str!("../README.md")]

mod client;
mod config;
mod flight;
mod prices;
mod tls;

pub use client::SpiceClient as Client;
pub use prices::{HistoricalPriceData, LatestPriceDetail, LatestPricesResponse};
pub use tls::new_tls_flight_channel;

// Further public exports and integrations
pub use futures::StreamExt;
