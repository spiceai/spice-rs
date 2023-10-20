#[cfg(test)]
mod tests {
    use spice_rs::*;
    use std::env;
    use std::error::Error;

    async fn test_get_client() -> Client {
        let api_key = env::var("API_KEY").expect("API_KEY not found");
        return new_spice_client(api_key)
            .await
            .expect("Failed to initiate spice client");
    }

    #[test]
    fn test_get_flight_info() {
        let client = test_get_client();l
        let descriptor = FlightDescriptor {
            descriptor_type: DescriptorType::Path,
            cmd: None,
            path: Some("test_path".to_string()),
        };
        let result = client.get_flight_info(&descriptor);
        assert!(result.is_err()); // Since the actual implementation is a placeholder
    }

    #[test]
    fn test_do_get() {
        let client = FlightClient::new("localhost");
        let ticket = Ticket {
            ticket: "test_ticket".to_string(),
        };
        let result = client.do_get(&ticket);
        assert!(result.is_err()); // Since the actual implementation is a placeholder
    }

    // Further tests for other flight-related functionalities
}
