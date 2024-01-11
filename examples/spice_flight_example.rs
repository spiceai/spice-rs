extern crate spice_rs;

use spice_rs::Client;
use spice_rs::StreamExt;

// Could be run from shell as 
// API_KEY='API-KEY' cargo run --example spice_flight_example
#[tokio::main]
async fn main() {

    let api_key =  std::env::var("API_KEY")
        // TODO: Replace API key below with your own API key value. Get it free at [spice.ai](https://spice.ai/)
        // Fallback to spice.ai demo api key (limited)
        .unwrap_or("313834|0666ecca421b4b33ba4d0dd2e90d6daa".to_string());
    
    let mut client = Client::new(api_key.as_str()).await.unwrap();

    let mut flight_data_stream = client.query(
        "SELECT number, to_timestamp(\"timestamp\"), transaction_count, gas_used FROM eth.recent_blocks LIMIT 10;")
        .await.expect("Error executing query");

    while let Some(batch) = flight_data_stream.next().await {
        match batch {
            Ok(batch) => {
                /* process batch */
                println!("Received {} rows and {} columns", batch.num_rows(), batch.num_columns());
                println!("{}", arrow_cast::pretty::pretty_format_batches(&[batch]).unwrap());
            },
            Err(e) => {
                /* handle error */
                panic!("Error while processing Flight data stream: {}", e)
            },
        };
    }
}