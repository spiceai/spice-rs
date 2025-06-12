# Rust Spice SDK

Rust SDK for Spice.ai

## Installation

Add Spice SDK

```bash
cargo add spiceai
```

## Usage

<!-- NOTE: If you're changing the code examples below, make sure you update `tests/readme_test.rs`. -->

### Usage with locally running [spice runtime](https://github.com/spiceai/spiceai)

Follow the [quickstart guide](https://github.com/spiceai/spiceai?tab=readme-ov-file#%EF%B8%8F-quickstart-local-machine) to install and run spice locally

```rust
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() {
  let client = ClientBuilder::new()
    .flight_url("http://localhost:50051")
    .build()
    .await
    .unwrap();

  let data = client.query("SELECT trip_distance, total_amount FROM taxi_trips ORDER BY trip_distance DESC LIMIT 10;").await;
}
```

### New client with <https://spice.ai> cloud

```rust
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() {
  let client = ClientBuilder::new()
    .api_key("API_KEY")
    .use_spiceai_cloud()
    .build()
    .await
    .unwrap();
}
```

### Arrow Query

SQL Query

```rust
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() {
  let client = ClientBuilder::new()
    .api_key("API_KEY")
    .use_spiceai_cloud()
    .build()
    .await
    .unwrap();

  let data = client.query("SELECT * FROM taxi_trips LIMIT 10;").await;
}
```

Parameterized SQL Query

```rust
use spiceai::ClientBuilder;

#[tokio::main]
async fn main() {
  let client = ClientBuilder::new()
    .api_key("API_KEY")
    .use_spiceai_cloud()
    .build()
    .await
    .unwrap();

  // Create a RecordBatch representing the values the parameters will bind https://docs.rs/arrow-flight/latest/arrow_flight/sql/client/struct.PreparedStatement.html#method.set_parameters
  let fields = vec![
    Arc::new(Field::new("$1", Int32, true)),
    Arc::new(Field::new("$2", Float64, true)),
  ];
  let columns = vec![
    Arc::new(Int32Array::from(vec![1])) as ArrayRef,
    Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
  ];

  let params = RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
    .expect("Failed to create RecordBatch");

  let data = client.query_with_params(
    "SELECT VendorID, tpep_pickup_datetime, fare_amount FROM taxi_trips WHERE VendorID == $1 and fare_amount > $2 ORDER BY fare_amount, tpep_pickup_datetime LIMIT 5;",
    Some(params),
  ).await
```

## Documentation

Check out our [Documentation](https://docs.spice.ai/sdks/rust-sdk) to learn more about how to use the Rust SDK.
