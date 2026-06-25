use crate::config::GenericError;
use base64::{Engine as _, engine::general_purpose};
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Once;

use tonic::transport::channel::{ClientTlsConfig, Endpoint};
use tonic::transport::{Channel, Identity};

static INIT: Once = Once::new();

pub fn system_tls_certificate() -> Result<tonic::transport::Certificate, GenericError> {
    // Load root certificates found in the platform's native certificate store.
    // Use the same pem format as spiceai cloud connector: https://github.com/spiceai/spiceai/blob/571007c4be89a2a9892e3bd0eb43f8bd28464a69/crates/flight_client/src/tls.rs#L47
    let cert_result = rustls_native_certs::load_native_certs();

    let mut pem = Vec::new();
    for cert in cert_result.certs {
        pem.write_all(b"-----BEGIN CERTIFICATE-----\n")?;
        pem.write_all(general_purpose::STANDARD.encode(cert.as_ref()).as_bytes())?;
        pem.write_all(b"\n-----END CERTIFICATE-----\n")?;
    }

    Ok(tonic::transport::Certificate::from_pem(pem))
}

/// Builder for creating TLS-enabled Flight channels.
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
/// use spiceai::tls::FlightChannelBuilder;
///
/// // Basic TLS (system roots)
/// let channel = FlightChannelBuilder::new("https://flight.spiceai.io")
///     .build()
///     .await?;
///
/// // With mTLS client certificate
/// let channel = FlightChannelBuilder::new("https://localhost:50051")
///     .with_client_certificate("client.crt", "client.key")
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct FlightChannelBuilder {
    url: String,
    client_cert_path: Option<PathBuf>,
    client_key_path: Option<PathBuf>,
    ca_cert_path: Option<PathBuf>,
}

impl FlightChannelBuilder {
    /// Creates a new builder for the given endpoint URL.
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client_cert_path: None,
            client_key_path: None,
            ca_cert_path: None,
        }
    }

    /// Sets a custom CA certificate file for server verification.
    ///
    /// When set, this CA is used instead of the system certificate store.
    #[must_use]
    pub fn with_ca_certificate(mut self, ca_path: impl Into<PathBuf>) -> Self {
        self.ca_cert_path = Some(ca_path.into());
        self
    }

    /// Sets the client certificate and key files for mutual TLS.
    ///
    /// Both files must be PEM-encoded. The certificate is presented to
    /// the server during the TLS handshake.
    #[must_use]
    pub fn with_client_certificate(
        mut self,
        cert_path: impl Into<PathBuf>,
        key_path: impl Into<PathBuf>,
    ) -> Self {
        self.client_cert_path = Some(cert_path.into());
        self.client_key_path = Some(key_path.into());
        self
    }

    /// Builds the Flight channel.
    ///
    /// The channel connects lazily: no connection is opened here, only on the
    /// first RPC. This lets an HTTP-only client be built without a reachable
    /// Flight endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint URL is invalid, the TLS configuration
    /// is invalid, or the certificate files cannot be read.
    pub async fn build(self) -> Result<Channel, GenericError> {
        let mut endpoint = Endpoint::from_str(&self.url)?;

        if self.url.starts_with("https://") {
            let cert = if let Some(ca_path) = &self.ca_cert_path {
                let ca_pem = tokio::fs::read(ca_path).await?;
                tonic::transport::Certificate::from_pem(ca_pem)
            } else {
                system_tls_certificate()?
            };
            let domain = self.url.trim_start_matches("https://");
            let domain = domain.split(':').next().unwrap_or(domain);
            let mut tls_config = ClientTlsConfig::new()
                .ca_certificate(cert)
                .domain_name(domain);

            if let (Some(cert_path), Some(key_path)) =
                (&self.client_cert_path, &self.client_key_path)
            {
                let cert_pem = tokio::fs::read(cert_path).await?;
                let key_pem = tokio::fs::read(key_path).await?;
                tls_config = tls_config.identity(Identity::from_pem(cert_pem, key_pem));
            }

            endpoint = endpoint.tls_config(tls_config)?;
        }

        Ok(endpoint.connect_lazy())
    }
}

/// Creates a new TLS-enabled Flight channel.
///
/// Equivalent to `FlightChannelBuilder::new(url).build()`.
pub async fn new_tls_flight_channel(url: &str) -> Result<Channel, GenericError> {
    FlightChannelBuilder::new(url).build().await
}

pub(crate) fn ensure_crypto_provider() {
    // Install the default AWS LC RS crypto provider for rusttls
    // Use the same provider as spiceai: https://github.com/spiceai/spiceai/blob/571007c4be89a2a9892e3bd0eb43f8bd28464a69/bin/spiced/src/main.rs#L74
    INIT.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::CryptoProvider::install_default(
                rustls::crypto::aws_lc_rs::default_provider(),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_tls_certificate_loads() {
        let result = system_tls_certificate();
        assert!(result.is_ok(), "should load system TLS certificates");
    }

    #[test]
    fn test_ensure_crypto_provider_does_not_panic() {
        // Should be safe to call multiple times
        ensure_crypto_provider();
        ensure_crypto_provider();
        ensure_crypto_provider();
    }

    #[tokio::test]
    async fn test_new_tls_flight_channel_http() {
        // A valid HTTP endpoint builds a lazy channel without connecting.
        let result = new_tls_flight_channel("http://localhost:12345").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_new_tls_flight_channel_https_invalid_host() {
        // A syntactically valid HTTPS endpoint builds a lazy channel; the host
        // is only resolved on the first RPC.
        let result = new_tls_flight_channel("https://invalid.nonexistent.host:443").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_endpoint_parsing_valid_https() {
        let endpoint = Endpoint::from_str("https://flight.spiceai.io");
        assert!(endpoint.is_ok());
    }

    #[test]
    fn test_endpoint_parsing_valid_http() {
        let endpoint = Endpoint::from_str("http://localhost:50051");
        assert!(endpoint.is_ok());
    }

    #[tokio::test]
    async fn test_new_tls_flight_channel_empty_url() {
        let result = new_tls_flight_channel("").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_new_tls_flight_channel_unreachable_port() {
        // Builds a lazy channel; an unreachable port only fails on the first RPC.
        let result = new_tls_flight_channel("http://127.0.0.1:1").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_new_tls_flight_channel_missing_scheme() {
        // The authority parses, so a lazy channel is built; any scheme/connection
        // problem surfaces on the first RPC rather than at build time.
        let result = new_tls_flight_channel("example.com:443").await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_endpoint_parsing_with_path() {
        let endpoint = Endpoint::from_str("https://flight.spiceai.io/path");
        assert!(endpoint.is_ok());
    }

    #[test]
    fn test_endpoint_parsing_with_port() {
        let endpoint = Endpoint::from_str("https://flight.spiceai.io:443");
        assert!(endpoint.is_ok());
    }

    #[test]
    fn test_endpoint_parsing_localhost_ipv4() {
        let endpoint = Endpoint::from_str("http://127.0.0.1:50051");
        assert!(endpoint.is_ok());
    }

    #[test]
    fn test_endpoint_parsing_localhost_ipv6() {
        let endpoint = Endpoint::from_str("http://[::1]:50051");
        assert!(endpoint.is_ok());
    }

    #[test]
    fn test_system_tls_certificate_pem_format() {
        let result = system_tls_certificate();
        assert!(
            result.is_ok(),
            "should load system TLS certificates in PEM format"
        );
    }

    #[test]
    fn test_crypto_provider_is_installed_after_ensure() {
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn test_flight_channel_builder_new() {
        let builder = FlightChannelBuilder::new("https://localhost:50051");
        assert_eq!(builder.url, "https://localhost:50051");
        assert!(builder.client_cert_path.is_none());
        assert!(builder.client_key_path.is_none());
    }

    #[test]
    fn test_flight_channel_builder_with_client_certificate() {
        let builder = FlightChannelBuilder::new("https://localhost:50051")
            .with_client_certificate("client.crt", "client.key");
        assert_eq!(
            builder.client_cert_path.as_deref(),
            Some(std::path::Path::new("client.crt"))
        );
        assert_eq!(
            builder.client_key_path.as_deref(),
            Some(std::path::Path::new("client.key"))
        );
    }
}
