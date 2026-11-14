use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::CertifiedIssuer;
use rcgen::ExtendedKeyUsagePurpose;
use rcgen::IsCa;
use rcgen::KeyPair;
use rcgen::KeyUsagePurpose;
use rustls::client::Resumption;
use rustls::crypto::ring;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::server::WebPkiClientVerifier;
use rustls::version;
use tempfile::TempDir;

pub struct Credentials {
    pub directory: TempDir,
    pub client: Arc<rustls::ClientConfig>,
    pub server: Arc<rustls::ServerConfig>,
}

fn ca() -> Result<CertifiedIssuer<'static, KeyPair>> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(CertifiedIssuer::self_signed(params, KeyPair::generate()?)?)
}

fn identity(
    ca: &CertifiedIssuer<'_, KeyPair>,
    name: &str,
    purpose: ExtendedKeyUsagePurpose,
) -> Result<(rcgen::Certificate, KeyPair)> {
    let mut params = CertificateParams::new(vec![format!("{name}.test")])?;
    params.extended_key_usages = vec![purpose];
    let key = KeyPair::generate()?;
    Ok((params.signed_by(&key, ca)?, key))
}

fn write(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())?;
    Ok(())
}

impl Credentials {
    pub fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let backhaul = ca()?;
        write(&directory.path().join("ca.pem"), &backhaul.pem())?;
        for (name, purpose) in [
            ("edge", ExtendedKeyUsagePurpose::ServerAuth),
            ("origin", ExtendedKeyUsagePurpose::ClientAuth),
        ] {
            let (cert, key) = identity(&backhaul, name, purpose)?;
            write(&directory.path().join(format!("{name}.pem")), &cert.pem())?;
            write(
                &directory.path().join(format!("{name}.key")),
                &key.serialize_pem(),
            )?;
        }
        // Relays receive no application identity or application CA configuration.
        let application = ca()?;
        let (backend, backend_key) =
            identity(&application, "backend", ExtendedKeyUsagePurpose::ServerAuth)?;
        let (client, client_key) =
            identity(&application, "client", ExtendedKeyUsagePurpose::ClientAuth)?;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(application.der().to_vec()))?;
        let provider = Arc::new(ring::default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots.clone()), provider.clone())
                .build()?;
        let key = |k: KeyPair| PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.serialize_der()));
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&version::TLS13])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![backend.der().clone()], key(backend_key))?;
        let mut client = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&version::TLS13])?
            .with_root_certificates(roots)
            .with_client_auth_cert(vec![client.der().clone()], key(client_key))?;
        client.resumption = Resumption::disabled();
        Ok(Self {
            directory,
            client: Arc::new(client),
            server: Arc::new(server),
        })
    }
}
