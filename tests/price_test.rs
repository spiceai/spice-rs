
#[cfg(test)]
mod tests {
    use spice_rs::*;
    use std::env;
    use std::path::Path;

    #[tokio::test]
    async fn test_get_prices() {
        let api_key = env::var("API_KEY").expect("API_KEY not set");
        let http_addr = "https://data.spiceai.io".to_string();
        let flight_addr = "grpc+tls://flight.spiceai.io".to_string();
        let firecache_addr = "grpc+tls://firecache.spiceai.io".to_string();

        let pair = "BTC-USD";

        let spice_client: Client = new_spice_client(api_key, http_addr, flight_addr, firecache_addr).await.expect("Failed to initiate spice client");

        let result = spice_client.prices.get_prices(pair).await;
        assert!(result.is_ok());

        // Code for evaluate results received
        // match spice_client.prices.get_prices(pair).await {
        //     Ok(r) => {
        //         println!("{:?}",r)
        //     }
        //     Err(e) => {
        //         println!("{:?}",e)
        //     }
        // }
    }

    #[tokio::test]
    async fn test_get_historical_prices() {
        let api_key = env::var("API_KEY").expect("API_KEY not set");
        let http_addr = "https://data.spiceai.io".to_string();
        let flight_addr = "grpc+tls://flight.spiceai.io".to_string();
        let firecache_addr = "grpc+tls://firecache.spiceai.io".to_string();

        let pair1 = "BTC-USD";
        let pair2 = "ETH-USD";

        let spice_client: Client = new_spice_client(api_key, http_addr, flight_addr, firecache_addr).await.expect("Failed to initiate spice client");

        let result = spice_client.prices.get_historical_prices(&[pair1, pair2], None, None, Some("1h")).await;
        assert!(result.is_ok());
        // Code for evaluate results received
        match spice_client.prices.get_historical_prices(&[pair1, pair2], Some(1697669766), Some(1697756166), Some("1h")).await {
        // match spice_client.prices.get_historical_prices(&[pair1, pair2], None, None, Some("1h")).await {
            Ok(r) => {
                assert!(r.contains_key("BTC-USD"));
                assert!(r.contains_key("ETH-USD"));
                // TODO: Check timepoints are between 1672531200000 & 1675209600000
            }
            Err(e) => {
                assert!(false, "Error: {}", e);
            }
        }
    }
    
    // Further tests for the client module
}