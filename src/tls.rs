use crate::config::GenericError;
use base64::{engine::general_purpose, Engine as _};
use std::str::FromStr;
use std::sync::Once;
use tonic::transport::channel::{ClientTlsConfig, Endpoint};
use tonic::transport::Channel;

static INIT: Once = Once::new();

pub fn system_tls_certificate() -> Result<tonic::transport::Certificate, GenericError> {
    // Load root certificates found in the platform’s native certificate store.
    let certs = rustls_native_certs::load_native_certs()?;

    let mut pem_data = String::new();
    for cert in certs {
        let encoded = general_purpose::STANDARD.encode(&cert.0);
        pem_data.push_str("-----BEGIN CERTIFICATE-----\n");
        for chunk in encoded.as_bytes().chunks(64) {
            pem_data.push_str(&String::from_utf8_lossy(chunk));
            pem_data.push('\n');
        }
        pem_data.push_str("-----END CERTIFICATE-----\n");
    }

    Ok(tonic::transport::Certificate::from_pem(pem_data))
}

pub async fn new_tls_flight_channel(https_url: &str) -> Result<Channel, GenericError> {
    let mut endpoint = Endpoint::from_str(https_url)?;

    if https_url.starts_with("https://") {
        let cert = system_tls_certificate()?;
        let tls_config = ClientTlsConfig::new()
            .ca_certificate(cert)
            .domain_name(https_url.trim_start_matches("https://"));
        endpoint = endpoint.tls_config(tls_config)?;
    }

    Ok(endpoint.connect().await?)
}

pub(crate) fn ensure_crypto_provider() {
    INIT.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::CryptoProvider::install_default(
                rustls::crypto::aws_lc_rs::default_provider(),
            );
        }
    });
}
