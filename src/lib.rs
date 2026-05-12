#![doc = include_str!("../README.md")]

mod client;
mod config;
mod flight;
pub mod query;
pub mod tls;
mod util;

pub use client::Error as SpiceClientError;
pub use client::SpiceClient as Client;
pub use client::SpiceClientBuilder as ClientBuilder;
pub use query::{
    QueryError, QueryInfo, QueryJob, QueryListResponse, QueryResult, QueryResultStream,
    QueryStatus, QuerySummary,
};

// Further public exports and integrations
pub use futures::StreamExt;
