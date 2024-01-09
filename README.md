# Rust Spice SDK

Rust SDK for Spice.ai

## Installation

Add the following to your `Cargo.toml`:

```toml
[dependencies]
spice-rs = { git = "https://github.com/spiceai/spice-rs", tag = "v1.0.2" }
```

## Usage
<!-- NOTE: If you're changing the code examples below, make sure you update `tests/readme_test.rs`. -->
### Arrow Query
**SQL Query**

```rust,ignore
use spice_rs::Client;

let mut client = Client::new("API_KEY").await;
let data = client.query("SELECT * FROM eth.recent_blocks LIMIT 10;").await;
```

### HTTP API
#### Prices

Get the supported pairs:

```rust,ignore
let supported_pairs = client.get_supported_pairs().await;
```

Get the latest price for a token pair:

```rust,ignore
let price_data = client.get_prices(&["BTC-USDC"]).await;
```

Get historical data:

```rust,ignore
let now = Utc::now();
let start = now.sub(Duration::seconds(3600));

let historical_price_data = client
  .get_historical_prices(&["BTC-USDC"], Some(start), Some(now), Option::None).await;
```

## Documentation
Check out our [Documentation](https://docs.spice.ai/sdks/rust-sdk) to learn more about how to use the Rust SDK.
