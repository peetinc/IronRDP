//! Browser-side TLS termination for the RDP connection.
//!
//! Upstream's web client never needed this: it speaks RDCleanPath to a Devolutions
//! Gateway, which terminates TLS itself. Our direct-WebSocket mode moved that job to
//! a plain relay instead of removing it — the relay still had to perform the TLS
//! handshake and hand the server's public key back to the browser so CredSSP could
//! bind to it.
//!
//! Terminating TLS here removes the relay from the trust boundary entirely: it sees
//! only ciphertext, never the NTLM/SPNEGO exchange, and it can no longer assert a
//! public key the browser has no way to check. It also frees the transport — any
//! byte pipe will do, which is what makes peer-to-peer rungs possible.
//!
//! [`rustls`] is driven over the existing `futures-io` stream (the gloo WebSocket)
//! through [`futures_rustls`], with the `ring` crypto provider — `aws-lc-rs`, rustls'
//! default, does not build for `wasm32-unknown-unknown`.

use std::io;
use std::sync::Arc;

use futures_rustls::TlsConnector;
pub(crate) use futures_rustls::client::TlsStream;
use futures_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use futures_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use futures_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};
use futures_util::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _};
use tracing::{debug, warn};

/// Performs the client side of the TLS handshake over `stream`.
///
/// Returns the TLS stream and the server's public key, in the same shape
/// [`ironrdp_tls::extract_tls_server_public_key`] produces for the native clients:
/// the raw contents of the certificate's `subjectPublicKey` BIT STRING, which is what
/// CredSSP binds to.
pub(crate) async fn upgrade<S>(stream: S, server_name: &str) -> io::Result<(TlsStream<S>, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let config = client_config()?;

    // The RDP server name is frequently an IP address or a NetBIOS name that is not a
    // valid DNS name. Certificates are not validated here (see `NoCertificateVerification`),
    // so the name only ever reaches the SNI extension; fall back to a placeholder rather
    // than failing the connection outright when it is not representable.
    let domain = match ServerName::try_from(server_name.to_owned()) {
        Ok(domain) => domain,
        Err(error) => {
            warn!(%error, server_name, "server name is not valid for SNI, using a placeholder");
            ServerName::try_from("rdp.invalid").expect("hardcoded valid name")
        }
    };

    let mut tls_stream = TlsConnector::from(config).connect(domain, stream).await?;

    tls_stream.flush().await?;

    let server_public_key = {
        use x509_cert::der::Decode as _;

        let (_, connection) = tls_stream.get_ref();

        debug!(
            version = ?connection.protocol_version(),
            cipher_suite = ?connection.negotiated_cipher_suite().map(|suite| suite.suite()),
            "TLS handshake completed in the browser"
        );

        let cert = connection
            .peer_certificates()
            .and_then(|certificates| certificates.first())
            .ok_or_else(|| io::Error::other("peer certificate is missing"))?;

        let cert = x509_cert::Certificate::from_der(cert).map_err(io::Error::other)?;

        cert.tbs_certificate()
            .subject_public_key_info()
            .subject_public_key
            .as_bytes()
            .ok_or_else(|| io::Error::other("subject public key is not a whole number of bytes"))?
            .to_vec()
    };

    debug!(
        len = server_public_key.len(),
        "Extracted server public key from the peer certificate"
    );

    Ok((tls_stream, server_public_key))
}

fn client_config() -> io::Result<Arc<ClientConfig>> {
    // Name the provider explicitly rather than relying on the process default: nothing
    // installs one in a wasm module, and `ClientConfig::builder()` panics when it cannot
    // resolve one.
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
        .with_no_client_auth();

    // Disable TLS resumption because it’s not supported by some services such as CredSSP.
    //
    // > The CredSSP Protocol does not extend the TLS wire protocol. TLS session resumption is not supported.
    //
    // source: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cssp/385a7489-d46b-464c-b224-f7340e308a5c
    config.resumption = rustls::client::Resumption::disabled();

    // No `KeyLogFile` here: it reads SSLKEYLOGFILE from the environment, which a wasm
    // module has no access to, and writing key material out of a browser tab is not
    // something we want a code path for.

    Ok(Arc::new(config))
}

/// Accepts any server certificate.
///
/// This preserves the behavior of the relay this replaces, which connected with
/// `rejectUnauthorized: false` — RDP hosts overwhelmingly present self-signed
/// certificates, and there is no trust store to validate them against. Authentication
/// comes from CredSSP binding to the public key extracted above, not from the chain.
///
/// Pinning or real chain validation is a separate, later tightening; it needs a place
/// to store per-host trust decisions, which the browser client does not have yet.
#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}
