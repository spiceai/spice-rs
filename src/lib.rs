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
pub use chrono;
pub use client::Error as SpiceClientError;
pub use client::SpiceClient as Client;
pub use client::SpiceClientBuilder as ClientBuilder;
pub use dataset::{
    DatasetError, DatasetRefreshMode, DatasetRefreshRequest, DatasetRefreshResponse,
};
pub use nsql::{
    NSQL_CONTEXT_MAX_LIMIT, NsqlContextRequest, NsqlError, NsqlField, NsqlRequest, NsqlResponse,
    NsqlSchema,
};
pub use params::{QueryParameter, QueryParameterError, QueryParameters};
pub use query::{
    QueryError, QueryInfo, QueryJob, QueryListResponse, QueryResult, QueryResultStream,
    QueryStatus, QuerySubmitOptions, QuerySummary,
};
pub use search::{SearchError, SearchMatch, SearchRequest, SearchResponse};
pub use status::{ComponentStatus, ConnectionDetails, StatusError};

// Further public exports and integrations
pub use futures::StreamExt;

/// The largest single Flight message, in bytes, a client accepts from the runtime unless
/// [`ClientBuilder::max_decoding_message_size`] says otherwise: 100 MiB, the limit the
/// Spice runtime applies to Flight messages it receives.
///
/// gRPC's own default is 4 MiB, which a result batch with many or wide rows can exceed.
pub const DEFAULT_MAX_DECODING_MESSAGE_SIZE: usize = 100 * 1024 * 1024;
