//! Certificate checks on the platforms where reqwest's platform verifier is
//! webpki over the system store (Linux and the BSDs). webpki takes a store
//! certificate only as an issuer, so a self-signed server certificate with the
//! CA flag set fails with `CaUsedAsEndEntity`, even when the user put that
//! exact certificate in the store or in `SSL_CERT_FILE`. OpenSSL, and so curl,
//! accepts a trusted self-signed certificate as the server certificate, CA
//! flag or not. The macOS and Windows verifiers do too, so this verifier does
//! the same.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{WebPkiServerVerifier, verify_server_name};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, OtherError,
    RootCertStore, SignatureScheme,
};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use webpki::Error as WebpkiError;

type CertDigest = [u8; 32];

/// webpki checks the dates before the CA flag, so a certificate that fails on
/// the flag is in date. The name is checked here, because webpki checks it
/// only after the chain.
#[derive(Debug)]
struct SystemStoreVerifier {
    webpki: Arc<WebPkiServerVerifier>,
    store_certs: Vec<CertDigest>,
}

impl SystemStoreVerifier {
    fn accept_store_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        server_name: &ServerName<'_>,
    ) -> Result<ServerCertVerified, TlsError> {
        verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
        debug!(server = %server_name.to_str(), "accepted a self-signed CA certificate from the system store");
        Ok(ServerCertVerified::assertion())
    }
}

impl ServerCertVerifier for SystemStoreVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        self.webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .or_else(|error| {
                if is_ca_used_as_end_entity(&error)
                    && self.store_certs.contains(&digest(end_entity))
                {
                    self.accept_store_cert(end_entity, server_name)
                } else {
                    Err(error)
                }
            })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

fn is_ca_used_as_end_entity(error: &TlsError) -> bool {
    matches!(
        error,
        TlsError::InvalidCertificate(CertificateError::Other(OtherError(inner)))
            if matches!(inner.downcast_ref::<WebpkiError>(), Some(WebpkiError::CaUsedAsEndEntity))
    )
}

fn digest(cert: &CertificateDer<'_>) -> CertDigest {
    Sha256::digest(cert).into()
}

/// Loads the store the way the platform verifier does, `SSL_CERT_FILE` and
/// `SSL_CERT_DIR` included. Fails when no certificate in it is usable.
pub(crate) fn system_store_config() -> Result<ClientConfig, TlsError> {
    let loaded = rustls_native_certs::load_native_certs();
    for error in &loaded.errors {
        warn!(%error, "could not load a system CA certificate");
    }
    let mut roots = RootCertStore::empty();
    let (_, ignored) = roots.add_parsable_certificates(loaded.certs.iter().cloned());
    if ignored > 0 {
        warn!(
            ignored,
            "system CA certificates ignored because they do not parse"
        );
    }
    let provider = Arc::new(aws_lc_rs::default_provider());
    let webpki =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()
            .map_err(|e| TlsError::General(e.to_string()))?;
    let verifier = SystemStoreVerifier {
        webpki,
        store_certs: loaded.certs.iter().map(digest).collect(),
    };
    Ok(ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rcgen::{BasicConstraints, CertificateParams, CertifiedKey, IsCa, KeyPair, date_time_ymd};
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use rustls::{ServerConfig, ServerConnection, StreamOwned};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::http::error_chain;
    use crate::providers::{Timeouts, http_client};

    /// Resolves to loopback without DNS, so the certificate can name a host.
    const SERVED_HOST: &str = "localhost";
    const OTHER_HOST: &str = "other.example";
    const STORE_FILE: &str = "store.pem";
    const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const REJECTED: &str = "invalid peer certificate";
    const EXPIRED_YEAR: i32 = 2000;

    enum Validity {
        Current,
        Expired,
    }

    enum Store {
        ServedCert,
        OtherCert,
    }

    enum Outcome {
        Accepted,
        Rejected,
    }

    fn self_signed(is_ca: IsCa, host: &str, validity: Validity) -> CertifiedKey<KeyPair> {
        let mut params = CertificateParams::new(vec![host.to_owned()]).unwrap();
        params.is_ca = is_ca;
        if let Validity::Expired = validity {
            params.not_before = date_time_ymd(EXPIRED_YEAR, 1, 1);
            params.not_after = date_time_ymd(EXPIRED_YEAR, 1, 2);
        }
        let signing_key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&signing_key).unwrap();
        CertifiedKey { cert, signing_key }
    }

    fn pem(certified: &CertifiedKey<KeyPair>) -> String {
        let base64 = STANDARD.encode(certified.cert.der());
        format!("-----BEGIN CERTIFICATE-----\n{base64}\n-----END CERTIFICATE-----\n")
    }

    /// Answers every request with an empty 200. A connection whose handshake
    /// fails ends at its first read.
    fn spawn_tls_server(certified: CertifiedKey<KeyPair>) -> u16 {
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into();
        let config = ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.into()], key)
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind((SERVED_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let connection = ServerConnection::new(Arc::clone(&config)).unwrap();
                thread::spawn(move || {
                    let mut reader = BufReader::new(StreamOwned::new(connection, stream));
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|read| read > 0) {
                        if line == "\r\n" {
                            let _ = reader.get_mut().write_all(RESPONSE);
                            return;
                        }
                        line.clear();
                    }
                });
            }
        });
        port
    }

    /// The returned folder holds the store, so it has to outlive the client.
    fn use_as_system_store(pem: &str) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE);
        fs::write(&path, pem).unwrap();
        // SAFETY: setting a variable is only sound while no other thread reads
        // the environment, and the runner is what holds that up: `just test`
        // runs `cargo nextest`, which gives every test its own process.
        unsafe {
            std::env::set_var("SSL_CERT_FILE", &path);
            std::env::remove_var("SSL_CERT_DIR");
        }
        dir
    }

    #[test_case(IsCa::Ca(BasicConstraints::Unconstrained), SERVED_HOST, Validity::Current, Store::ServedCert, Outcome::Accepted ; "ca_in_store")]
    #[test_case(IsCa::NoCa,                                SERVED_HOST, Validity::Current, Store::ServedCert, Outcome::Accepted ; "leaf_in_store")]
    #[test_case(IsCa::Ca(BasicConstraints::Unconstrained), SERVED_HOST, Validity::Current, Store::OtherCert,  Outcome::Rejected ; "ca_not_in_store")]
    #[test_case(IsCa::Ca(BasicConstraints::Unconstrained), OTHER_HOST,  Validity::Current, Store::ServedCert, Outcome::Rejected ; "ca_for_another_name")]
    #[test_case(IsCa::Ca(BasicConstraints::Unconstrained), SERVED_HOST, Validity::Expired, Store::ServedCert, Outcome::Rejected ; "expired_ca_in_store")]
    #[tokio::test]
    async fn self_signed_server_cert_is_trusted_only_as_stored(
        is_ca: IsCa,
        cert_host: &str,
        validity: Validity,
        store: Store,
        expected: Outcome,
    ) {
        let served = self_signed(is_ca, cert_host, validity);
        let stored = match store {
            Store::ServedCert => pem(&served),
            Store::OtherCert => pem(&self_signed(IsCa::NoCa, SERVED_HOST, Validity::Current)),
        };
        let _store = use_as_system_store(&stored);
        let port = spawn_tls_server(served);

        let result = http_client(Timeouts::default())
            .get(format!("https://{SERVED_HOST}:{port}/"))
            .send()
            .await;

        match expected {
            Outcome::Accepted => assert!(result.unwrap().status().is_success()),
            Outcome::Rejected => {
                let chain = error_chain(&result.unwrap_err());
                assert!(chain.contains(REJECTED), "got: {chain}");
            }
        }
    }
}
