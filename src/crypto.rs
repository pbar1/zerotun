use std::slice;
use std::sync::Arc;

use rustls::SignatureScheme;
use rustls::crypto::CryptoProvider;
use rustls::crypto::KeyProvider;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::crypto::aws_lc_rs;
use rustls::crypto::aws_lc_rs::cipher_suite;
use rustls::crypto::aws_lc_rs::kx_group;
use rustls::pki_types::PrivateKeyDer;
use rustls::quic::Suite;
use rustls::sign::SigningKey;

pub fn provider() -> CryptoProvider {
    let mut provider = aws_lc_rs::default_provider();
    let signatures = provider
        .signature_verification_algorithms
        .mapping
        .iter()
        .find(|(scheme, _)| *scheme == SignatureScheme::ML_DSA_65)
        .expect("AWS-LC supports ML-DSA-65");
    // Restrict both certificate-chain signatures and TLS CertificateVerify signatures.
    provider.signature_verification_algorithms = WebPkiSupportedAlgorithms {
        all: signatures.1,
        mapping: slice::from_ref(signatures),
    };
    provider.kx_groups = vec![kx_group::MLKEM768];
    provider.cipher_suites = vec![cipher_suite::TLS13_AES_256_GCM_SHA384];
    provider.key_provider = &MlDsa65Keys;
    provider
}

pub fn initial_suite() -> Suite {
    // QUIC mandates AES-128 for publicly derivable Initial protection. Keep it separate
    // from the negotiated cipher suites so application traffic always uses AES-256.
    cipher_suite::TLS13_AES_128_GCM_SHA256
        .tls13()
        .expect("TLS 1.3 cipher suite")
        .quic_suite()
        .expect("QUIC cipher suite")
}

#[derive(Debug)]
struct MlDsa65Keys;

impl KeyProvider for MlDsa65Keys {
    fn load_private_key(
        &self,
        key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn SigningKey>, rustls::Error> {
        let key = aws_lc_rs::default_provider()
            .key_provider
            .load_private_key(key_der)?;
        if key.choose_scheme(&[SignatureScheme::ML_DSA_65]).is_none() {
            return Err(rustls::Error::General(
                "backhaul identity keys must use ML-DSA-65".into(),
            ));
        }
        Ok(key)
    }
}
