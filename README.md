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

### Search

`search()` runs vector similarity, keyword, and hybrid search against datasets that have an embedding column and a loaded embedding model. Like dataset refresh, it uses the Spice HTTP API, so configure `http_url()`.

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
      SearchRequest::new("tickets to Tokyo")
        .with_datasets(["app_messages"])
        .with_limit(3),
    )
    .await?;

  println!("{} matches in {}ms", response.len(), response.duration_ms);
  for m in &response {
    println!("{} {} {:?}", m.dataset, m.score, m.matches);
  }
  Ok(())
}
```

Only the query text is required. `with_datasets` restricts the search — omit it to search every dataset with an embedding column. `with_limit` caps matches per dataset, `with_where` applies an SQL predicate before the search, and `with_additional_columns` names extra columns to return. `with_keywords` pre-filters the embedding column with a lexical search before the vector search runs, making the search hybrid:

```rust,no_run
use spiceai::SearchRequest;

let request = SearchRequest::new("tickets to Tokyo")
  .with_where("city = 'Tokyo'")
  .with_additional_columns(["timestamp"])
  .with_keywords(["plane", "tickets"]);
```

Each `SearchMatch` carries the `dataset` it was found in, its similarity `score`, the matched column values in `matches`, the row's `primary_key`, the columns requested via `with_additional_columns` in `data`, and any `metadata`. The runtime omits the last four when empty; they deserialize to empty maps, so they can be read without a guard.

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

## Documentation

Check out our [Documentation](https://docs.spice.ai/sdks/rust-sdk) to learn more about how to use the Rust SDK.
