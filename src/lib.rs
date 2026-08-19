#![doc = include_str!("../README.md")]

pub mod active_query;
mod client;
mod config;
mod dataset;
mod flight;
pub mod nsql;
mod params;
pub mod query;
mod redirect;
pub mod search;
pub mod status;
pub mod tls;
mod util;

pub use active_query::{ActiveQuery, ActiveQueryError, ActiveQueryList, CancelActiveQueryResponse};
pub use arrow;
pub use client::Error as SpiceClientError;
pub use client::SpiceClient as Client;
pub use client::SpiceClientBuilder as ClientBuilder;
pub use dataset::{
    DatasetError, DatasetRefreshMode, DatasetRefreshRequest, DatasetRefreshResponse,
};
pub use nsql::{NsqlError, NsqlField, NsqlRequest, NsqlResponse, NsqlSchema};
pub use params::{QueryParameter, QueryParameterError, QueryParameters};
pub use query::{
    QueryError, QueryInfo, QueryJob, QueryListResponse, QueryResult, QueryResultStream,
    QueryStatus, QuerySubmitOptions, QuerySummary,
};
pub use search::{SearchError, SearchMatch, SearchRequest, SearchResponse};
pub use status::{ComponentStatus, ConnectionDetails, StatusError};

// Further public exports and integrations
pub use futures::StreamExt;
