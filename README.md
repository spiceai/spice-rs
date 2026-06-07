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

## Documentation

Check out our [Documentation](https://docs.spice.ai/sdks/rust-sdk) to learn more about how to use the Rust SDK.
