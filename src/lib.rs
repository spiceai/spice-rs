#![doc = include_str!("../README.md")]

mod client;
mod config;
mod dataset;
mod flight;
mod params;
pub mod query;
mod tls;
mod util;

pub use arrow;
pub use client::Error as SpiceClientError;
pub use client::SpiceClient as Client;
pub use client::SpiceClientBuilder as ClientBuilder;
pub use dataset::{
    DatasetError, DatasetRefreshMode, DatasetRefreshRequest, DatasetRefreshResponse,
};
pub use params::{QueryParameter, QueryParameterError, QueryParameters};
pub use query::{
    QueryError, QueryInfo, QueryJob, QueryListResponse, QueryResult, QueryResultStream,
    QueryStatus, QuerySummary,
};

// Further public exports and integrations
pub use futures::StreamExt;
