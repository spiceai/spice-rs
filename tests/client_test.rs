#[cfg(test)]
mod tests {
    use futures::stream::StreamExt;
    use spiceai::{Client, ClientBuilder};
    use std::env;
    use std::path::Path;

    async fn new_cloud_client() -> Client {
        dotenv::from_path(Path::new(".env.local")).ok();
        let api_key = env::var("API_KEY").expect("API_KEY not found");
        ClientBuilder::new()
            .api_key(&api_key)
            .use_spiceai_cloud()
            .build()
            .await
            .expect("Failed to create client")
    }

    #[tokio::test]
    async fn test_new_client_builder() {
        new_cloud_client().await;
    }

    #[tokio::test]
    async fn test_query() {
        let mut spice_client = new_cloud_client().await;
        match spice_client
            .query(
                r#"select VendorID, trip_distance, tpep_pickup_datetime from taxi_trips limit 10;"#,
            )
            .await
        {
            Ok(mut flight_data_stream) => {
                // Read back RecordBatches
                while let Some(batch) = flight_data_stream.next().await {
                    match batch {
                        Ok(batch) => {
                            assert_eq!(batch.num_columns(), 3);
                            assert_eq!(batch.num_rows(), 10);
                        }
                        Err(e) => {
                            panic!("Error: {e}")
                        }
                    };
                }
            }
            Err(e) => {
                panic!("Error: {e}");
            }
        };
    }
}
