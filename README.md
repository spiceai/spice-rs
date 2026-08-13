# Rust Spice SDK

Rust SDK for Spice.ai.

## Installation

Add the SDK:

```bash
cargo add spiceai
```

## Usage

### Query a local Spice runtime

Follow the [quickstart guide](https://github.com/spiceai/spiceai?tab=readme-ov-file#%EF%B8%8F-quickstart-local-machine) to install and run Spice locally.

```rust,no_run
use spiceai::{ClientBuilder, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new().build().await?;

  let mut stream = client
    .sql(
      "SELECT trip_distance, total_amount FROM taxi_trips ORDER BY trip_distance DESC LIMIT 10;",
    )
    .await?;

  while let Some(batch) = stream.next().await {
    println!("rows: {}", batch?.num_rows());
  }

  Ok(())
}
```

### Use Arrow types re-exported by the SDK

The SDK re-exports `arrow` as `spiceai::arrow`, which keeps your Arrow types aligned with the SDK's public API.

```rust,no_run
use spiceai::{arrow::array::Float64Array, ClientBuilder, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new().build().await?;

  let mut stream = client
    .sql("SELECT trip_distance FROM taxi_trips ORDER BY trip_distance DESC LIMIT 1;")
    .await?;

  if let Some(batch) = stream.next().await {
    let batch = batch?;
    let values = batch
      .column(0)
      .as_any()
      .downcast_ref::<Float64Array>()
      .expect("trip_distance should be Float64");

    println!("longest trip: {}", values.value(0));
  }

  Ok(())
}
```

### Parameterized queries

For common scalar bindings, use `QueryParameters`. For any Arrow data type, wrap a one-element Arrow array with `QueryParameter::array(...)`. For advanced Arrow parameter batches, use `Client::sql_with_params`.

```rust,no_run
use spiceai::{ClientBuilder, QueryParameters, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new().build().await?;

  let mut stream = client
    .sql_with_bindings(
      "SELECT VendorID, fare_amount FROM taxi_trips WHERE VendorID = $1 AND fare_amount > $2 LIMIT 5;",
      QueryParameters::new().push(1_i32).push(1.0_f64),
    )
    .await?;

  while let Some(batch) = stream.next().await {
    println!("rows: {}", batch?.num_rows());
  }

  Ok(())
}
```

### Connect to Spice.ai Cloud

```rust,no_run
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new()
    .api_key("API_KEY")
    .use_spiceai_cloud()
    .build()
    .await?;

  let _ = client;
  Ok(())
}
```

### Async query jobs and dataset refresh

Async query management and dataset refresh use the Spice HTTP API, so configure `http_url()` in addition to the Flight endpoint when needed.

```rust,no_run
use spiceai::{ClientBuilder, DatasetRefreshMode, DatasetRefreshRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new()
    .http_url("http://localhost:8090")
    .build()
    .await?;

  let job = client.query("SELECT * FROM large_table").await?;
  println!("query status: {}", job.status().await?);

  let response = client
    .refresh_dataset_with_options(
      "taxi_trips",
      DatasetRefreshRequest::new().with_refresh_mode(DatasetRefreshMode::Full),
    )
    .await?;

  println!("{}", response.message);
  Ok(())
}
```

Async queries also accept positional bindings (`$1`, `$2`, ...) and submit options. Use `query_with_bindings` for the common parameterized case, or `query_with_options` to also set an execution `timeout_seconds` or a `maximum_size` cap on the materialized result.

```rust,no_run
use spiceai::{ClientBuilder, QueryParameters, QuerySubmitOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new()
    .http_url("http://localhost:8090")
    .build()
    .await?;

  let job = client
    .query_with_options(
      "SELECT * FROM large_table WHERE status = $1 AND created_at > $2",
      QuerySubmitOptions::new()
        .bindings(QueryParameters::new().push("active").push("2025-01-01"))
        .timeout_seconds(300)
        .maximum_size(100_000_000),
    )
    .await?;

  let result = job.wait().await?;
  println!("completed with {} rows", result.total_rows);
  Ok(())
}
```

### List and cancel running queries

`active_queries()` reports the synchronous queries currently running in the caller's scope — the ones started by `sql()`, FlightSQL, `/v1/sql`, NSQL, and search — and `cancel_active_query()` stops one by id.

The runtime does not hand a query's id back to the client that submitted it, so the two are used together: list to find the query, then cancel it.

Two boundaries apply, and a query is reachable only inside both.

**One runtime instance.** The runtime tracks active synchronous queries in memory, per instance, and these endpoints report only what the instance answering them knows. A `Client` configures its Flight and HTTP endpoints independently, so behind a load balancer the query submitted over Flight may be running on a different instance than the one answering here — it will not be listed, and its id reports as not found. Point `http_url()` at the instance running the query.

**One authenticated principal**, not a `Client` instance. The principal is whatever credential the runtime authenticates — an API key or a client certificate — so every client presenting the same credential lists and cancels the same queries. Only requests for which the runtime establishes no principal at all share the `public` scope. A query outside the caller's scope is reported as if it did not exist.

> **Runtime version.** Principal scoping on these two endpoints landed in [spiceai/spiceai#12841](https://github.com/spiceai/spiceai/pull/12841) and is in no runtime release up to and including `v2.1.5`. Against an earlier runtime both calls operate on every active query the instance holds, for any caller with write access. Check your runtime version before relying on the scope described above.

```rust,no_run
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new()
    .http_url("http://localhost:8090")
    .build()
    .await?;

  let active = client.active_queries().await?;
  println!("{} queries running", active.total_count);

  for query in &active.queries {
    println!("{} [{}] {}", query.query_id, query.protocol, query.sql_preview);
  }

  if let Some(query) = active.queries.first() {
    let cancelled = client.cancel_active_query(&query.query_id).await?;
    println!("{} is now {}", cancelled.query_id, cancelled.status);
  }

  Ok(())
}
```

To cancel an *async query job* instead, use `cancel_query()` — see [Async query jobs](#async-query-jobs-and-dataset-refresh) above.

### Search

`search` finds documents similar to a piece of text using the runtime's `/v1/search` endpoint. It runs against datasets that have an embedding column and a loaded embedding model — see [Search & Retrieval](https://docs.spice.ai/features/search-and-retrieval) for how to configure them. Like dataset refresh, it uses the HTTP API, so `http_url()` must be configured.

```rust,no_run
use spiceai::{ClientBuilder, SearchRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let client = ClientBuilder::new()
    .http_url("http://localhost:8090")
    .build()
    .await?;

  let response = client
    .search(
      SearchRequest::new("tokyo plane tickets")
        .with_datasets(["app_messages"])
        .with_limit(3)
        .with_additional_columns(["timestamp"]),
    )
    .await?;

  println!("{} matches in {}ms", response.len(), response.duration_ms);
  for m in response {
    println!("{} {} {:?}", m.score, m.dataset, m.matches);
  }
  Ok(())
}
```

Adding `with_keywords([...])` runs a lexical pass alongside the vector pass, which the runtime combines into a single hybrid ranking. `with_where("user_id = 42")` filters candidate rows with a SQL predicate.

Each `SearchMatch` carries `dataset`, `score` (higher is more similar), `matches` (matched values keyed by source column — a list per column, since one column can contribute several chunks to a match), `primary_key`, `data`, and `metadata`.

## Documentation

Check out our [Documentation](https://docs.spice.ai/sdks/rust-sdk) to learn more about how to use the Rust SDK.
