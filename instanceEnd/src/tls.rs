use std::sync::{Arc, Once};

static INSTALL_PROVIDER: Once = Once::new();

pub fn install_crypto_provider() {
    INSTALL_PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn bundled_root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

fn client_config() -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS protocol versions")
        .with_root_certificates(bundled_root_store())
        .with_no_client_auth()
}

pub fn http_client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    // Minimal OpenWrt and musl systems may not provide a system CA bundle.
    reqwest::Client::builder().tls_backend_preconfigured(client_config())
}

pub fn http_client() -> reqwest::Client {
    http_client_builder()
        .build()
        .expect("bundled TLS roots produce a valid HTTP client")
}

#[cfg(test)]
mod tests {
    use super::{bundled_root_store, http_client_builder};

    #[test]
    fn uses_the_complete_bundled_webpki_root_store() {
        let roots = bundled_root_store();
        assert!(!roots.is_empty());
        assert_eq!(roots.len(), webpki_roots::TLS_SERVER_ROOTS.len());
    }

    #[test]
    fn bundled_roots_build_an_http_client() {
        http_client_builder().build().unwrap();
    }
}
