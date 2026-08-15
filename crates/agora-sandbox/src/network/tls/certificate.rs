use anyhow::{Context, Result, bail};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PublicKeyData,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, pem::PemObject};
use rustls::sign::CertifiedKey;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration as StdDuration, Instant};
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::{FromDer, X509Certificate};

const LEAF_VALIDITY: Duration = Duration::days(1);
const CLOCK_SKEW: Duration = Duration::minutes(5);
const CA_VALIDITY: Duration = Duration::days(3650);
const CERTIFICATE_CACHE_VALIDITY: StdDuration = StdDuration::from_secs(60 * 60);

pub(in crate::network) fn generate_ca(
    certificate_path: &Path,
    private_key_path: &Path,
) -> Result<()> {
    if certificate_path == private_key_path {
        bail!("TLS CA certificate and private key paths must differ");
    }
    let key = KeyPair::generate().context("failed to generate TLS CA private key")?;
    let mut params = CertificateParams::new(Vec::new())
        .context("failed to create TLS CA certificate parameters")?;
    let now = OffsetDateTime::now_utc();
    params.not_before = now - CLOCK_SKEW;
    params.not_after = now + CA_VALIDITY;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "Agora Sandbox CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params
        .self_signed(&key)
        .context("failed to generate TLS CA certificate")?;

    write_ca_file(private_key_path, key.serialize_pem().as_bytes())?;
    write_ca_file(certificate_path, certificate.pem().as_bytes())?;
    Ok(())
}

fn write_ca_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create TLS CA directory {}", parent.display()))?;

    let temporary = parent.join(format!(".agora-ca-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).with_context(|| {
            format!(
                "failed to create temporary TLS CA file for {}",
                path.display()
            )
        })?;
        file.write_all(contents)
            .with_context(|| format!("failed to write TLS CA file {}", path.display()))?;
        #[cfg(unix)]
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to secure TLS CA file {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync TLS CA file {}", path.display()))?;
        drop(file);
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to publish TLS CA file {}", path.display()))?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("failed to sync TLS CA directory {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(in crate::network) struct TlsAuthority {
    issuer: Issuer<'static, KeyPair>,
    trust_anchor_der: Vec<u8>,
    cache: Mutex<CertificateCache>,
}

impl fmt::Debug for TlsAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsAuthority")
            .field("trust_anchor_der_len", &self.trust_anchor_der.len())
            .field("cache_capacity", &lock(&self.cache).capacity)
            .finish_non_exhaustive()
    }
}

impl TlsAuthority {
    pub(in crate::network) fn from_pem(
        certificate_pem: &[u8],
        private_key_pem: &[u8],
        cache_capacity: usize,
    ) -> Result<Self> {
        if cache_capacity == 0 {
            bail!("TLS certificate cache capacity must be greater than zero");
        }
        let certificate_der = parse_ca_certificate(certificate_pem)?;
        let private_key = std::str::from_utf8(private_key_pem)
            .context("TLS CA private key is not valid UTF-8 PEM")?;
        let private_key = KeyPair::from_pem(private_key).context("failed to parse TLS CA key")?;
        validate_ca(&certificate_der, &private_key)?;
        let issuer = Issuer::from_ca_cert_der(&certificate_der, private_key)
            .context("failed to initialize TLS CA issuer")?;

        Ok(Self {
            issuer,
            trust_anchor_der: certificate_der.to_vec(),
            cache: Mutex::new(CertificateCache::new(cache_capacity)),
        })
    }

    pub(super) fn issue(&self, identity: &str) -> Result<Arc<IssuedCertificate>> {
        let identity = normalize_identity(identity)?;
        let mut cache = lock(&self.cache);
        if let Some(certificate) = cache.get(&identity, Instant::now()) {
            return Ok(certificate);
        }

        let signing_key = KeyPair::generate().context("failed to generate TLS leaf key")?;
        let mut params = CertificateParams::new(vec![identity.clone()])
            .context("failed to create TLS leaf certificate parameters")?;
        let now = OffsetDateTime::now_utc();
        params.not_before = now - CLOCK_SKEW;
        params.not_after = now + LEAF_VALIDITY;
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, identity.clone());
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let certificate = params
            .signed_by(&signing_key, &self.issuer)
            .context("failed to sign TLS leaf certificate")?;
        let certificate_der = certificate.der().clone();
        let private_key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let certified_key = CertifiedKey::from_der(
            vec![certificate_der.clone()],
            private_key.into(),
            &rustls::crypto::aws_lc_rs::default_provider(),
        )
        .context("failed to prepare TLS leaf certificate")?;
        let issued = Arc::new(IssuedCertificate {
            certified_key: Arc::new(certified_key),
        });
        cache.insert(identity, Arc::clone(&issued), Instant::now());
        Ok(issued)
    }

    pub(in crate::network) fn trust_anchor_der(&self) -> Vec<u8> {
        self.trust_anchor_der.clone()
    }

    #[cfg(test)]
    pub(super) fn cache_len(&self) -> usize {
        lock(&self.cache).entries.len()
    }

    #[cfg(test)]
    pub(super) fn expire_for_test(&self, identity: &str) {
        lock(&self.cache)
            .entries
            .get_mut(identity)
            .expect("certificate must be cached")
            .expires_at = Instant::now();
    }
}

pub(super) struct IssuedCertificate {
    certified_key: Arc<CertifiedKey>,
}

impl IssuedCertificate {
    pub(super) fn certified_key(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.certified_key)
    }

    #[cfg(test)]
    pub(super) fn certificate_der(&self) -> &[u8] {
        self.certified_key.cert[0].as_ref()
    }
}

struct CertificateCache {
    capacity: usize,
    entries: HashMap<String, CachedCertificate>,
    order: VecDeque<String>,
}

struct CachedCertificate {
    certificate: Arc<IssuedCertificate>,
    expires_at: Instant,
}

impl CertificateCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, identity: &str, now: Instant) -> Option<Arc<IssuedCertificate>> {
        if self
            .entries
            .get(identity)
            .is_some_and(|certificate| certificate.expires_at <= now)
        {
            self.entries.remove(identity);
            self.remove_from_order(identity);
            return None;
        }
        let certificate = Arc::clone(&self.entries.get(identity)?.certificate);
        self.touch(identity);
        Some(certificate)
    }

    fn insert(&mut self, identity: String, certificate: Arc<IssuedCertificate>, now: Instant) {
        self.entries.insert(
            identity.clone(),
            CachedCertificate {
                certificate,
                expires_at: now + CERTIFICATE_CACHE_VALIDITY,
            },
        );
        self.touch(&identity);
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn touch(&mut self, identity: &str) {
        self.remove_from_order(identity);
        self.order.push_back(identity.to_string());
    }

    fn remove_from_order(&mut self, identity: &str) {
        if let Some(index) = self.order.iter().position(|entry| entry == identity) {
            self.order.remove(index);
        }
    }
}

fn parse_ca_certificate(pem: &[u8]) -> Result<CertificateDer<'static>> {
    let mut certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse TLS CA certificate PEM")?;
    if certificates.len() != 1 {
        bail!("TLS CA certificate PEM must contain exactly one certificate");
    }
    Ok(certificates.remove(0))
}

fn validate_ca(certificate_der: &CertificateDer<'_>, private_key: &KeyPair) -> Result<()> {
    let (remainder, certificate) = X509Certificate::from_der(certificate_der)
        .map_err(|error| anyhow::anyhow!("failed to parse TLS CA certificate: {error}"))?;
    if !remainder.is_empty() {
        bail!("TLS CA certificate contains trailing data");
    }
    let is_ca = certificate
        .basic_constraints()
        .context("failed to read TLS CA basic constraints")?
        .is_some_and(|constraints| constraints.value.ca);
    if !is_ca {
        bail!("TLS CA certificate is not a certificate authority");
    }
    let can_sign = certificate
        .key_usage()
        .context("failed to read TLS CA key usage")?
        .is_some_and(|usage| usage.value.key_cert_sign());
    if !can_sign {
        bail!("TLS CA certificate cannot sign certificates");
    }
    if certificate.public_key().raw != private_key.subject_public_key_info() {
        bail!("TLS CA private key does not match the certificate");
    }
    Ok(())
}

fn normalize_identity(identity: &str) -> Result<String> {
    let identity = identity.trim().trim_end_matches('.').to_ascii_lowercase();
    if identity.is_empty() {
        bail!("TLS certificate identity must not be empty");
    }
    if identity.parse::<IpAddr>().is_ok() {
        return Ok(identity);
    }
    let Some(registrable_domain) = psl::domain_str(&identity) else {
        return Ok(identity);
    };
    if registrable_domain == identity {
        return Ok(identity);
    }
    if let Some((_, parent)) = identity.split_once('.') {
        return Ok(format!("*.{parent}"));
    }
    Ok(identity)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
