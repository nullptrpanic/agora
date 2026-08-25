use super::{TlsAuthority, generate_ca, normalize_identity};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use std::net::{IpAddr, Ipv4Addr};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::{FromDer, X509Certificate};

#[test]
fn ca_generation_creates_parent_directories_and_replaces_existing_outputs() {
    let directory = std::env::temp_dir().join(format!(
        "agora-sandbox-generate-ca-{}",
        uuid::Uuid::new_v4()
    ));
    let certificate = directory.join("nested/ca.pem");
    let private_key = directory.join("nested/ca-key.pem");

    generate_ca(&certificate, &private_key).unwrap();
    let first_certificate = std::fs::read_to_string(&certificate).unwrap();
    let first_private_key = std::fs::read_to_string(&private_key).unwrap();
    assert!(first_certificate.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(first_private_key.starts_with("-----BEGIN PRIVATE KEY-----"));

    generate_ca(&certificate, &private_key).unwrap();

    assert_ne!(
        std::fs::read_to_string(&certificate).unwrap(),
        first_certificate
    );
    assert_ne!(
        std::fs::read_to_string(&private_key).unwrap(),
        first_private_key
    );
    #[cfg(unix)]
    {
        assert_eq!(
            certificate.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            private_key.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ca_generation_rejects_one_path_for_both_outputs() {
    let path = std::env::temp_dir().join(format!(
        "agora-sandbox-duplicate-ca-path-{}",
        uuid::Uuid::new_v4()
    ));

    let error = generate_ca(&path, &path).unwrap_err();

    assert!(error.to_string().contains("paths must differ"));
    assert!(!path.exists());
}

#[test]
fn ca_generation_failure_does_not_truncate_an_existing_certificate() {
    let directory =
        std::env::temp_dir().join(format!("agora-sandbox-failed-ca-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let certificate = directory.join("ca.pem");
    let invalid_private_key = directory.join("key-directory");
    std::fs::write(&certificate, b"existing certificate").unwrap();
    std::fs::create_dir(&invalid_private_key).unwrap();

    assert!(generate_ca(&certificate, &invalid_private_key).is_err());
    assert_eq!(
        std::fs::read(&certificate).unwrap(),
        b"existing certificate"
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn ca_generation_allows_a_symlinked_output_directory() {
    let directory = tempfile::tempdir().unwrap();
    let external = directory.path().join("external");
    let output = directory.path().join("output");
    std::fs::create_dir(&external).unwrap();
    std::os::unix::fs::symlink(&external, &output).unwrap();

    generate_ca(&output.join("ca.pem"), &output.join("ca-key.pem"))
        .expect("a symbolic-link CA output directory should follow native path resolution");

    assert!(external.join("ca.pem").is_file());
    assert!(external.join("ca-key.pem").is_file());
}

#[test]
fn authority_rejects_malformed_ca_material() {
    let error = TlsAuthority::from_pem(b"not a certificate", b"not a key", 4).unwrap_err();

    assert!(error.to_string().contains("CA certificate"));
}

#[test]
fn authority_debug_and_input_validation_are_explicit() {
    let (certificate, key) = test_ca();
    let authority = TlsAuthority::from_pem(certificate.as_bytes(), key.as_bytes(), 4).unwrap();
    let debug = format!("{authority:?}");

    assert!(debug.contains("TlsAuthority"));
    assert!(debug.contains("trust_anchor_der_len"));
    assert!(debug.contains("cache_capacity: 4"));
    assert!(
        TlsAuthority::from_pem(certificate.as_bytes(), key.as_bytes(), 0)
            .unwrap_err()
            .to_string()
            .contains("capacity must be greater than zero")
    );
    assert_eq!(normalize_identity("LOCALHOST.").unwrap(), "localhost");
    assert!(normalize_identity(" . ").is_err());
}

#[test]
fn authority_rejects_a_private_key_that_does_not_match_the_ca() {
    let (certificate, _) = test_ca();
    let other_key = KeyPair::generate().unwrap().serialize_pem();

    let error =
        TlsAuthority::from_pem(certificate.as_bytes(), other_key.as_bytes(), 4).unwrap_err();

    assert!(error.to_string().contains("does not match"));
}

#[test]
fn authority_uses_public_suffix_aware_dns_names_and_exact_ip_addresses() {
    let (certificate, key) = test_ca();
    let authority = TlsAuthority::from_pem(certificate.as_bytes(), key.as_bytes(), 4).unwrap();

    let subdomain = authority.issue("www.baidu.com").unwrap();
    let registrable = authority.issue("foo.co.uk").unwrap();
    let private_suffix = authority.issue("bar.foo.appspot.com").unwrap();
    let ip = authority.issue("127.0.0.1").unwrap();

    assert_eq!(
        subject_alt_names(subdomain.certificate_der()),
        vec!["*.baidu.com"]
    );
    assert_eq!(
        subject_alt_names(registrable.certificate_der()),
        vec!["foo.co.uk"]
    );
    assert_eq!(
        subject_alt_names(private_suffix.certificate_der()),
        vec!["*.foo.appspot.com"]
    );
    assert_eq!(subject_alt_names(ip.certificate_der()), vec!["127.0.0.1"]);
    assert_eq!(authority.trust_anchor_der(), ca_der(&certificate));
}

#[test]
fn authority_reuses_cached_certificates_and_bounds_the_cache() {
    let (certificate, key) = test_ca();
    let authority = TlsAuthority::from_pem(certificate.as_bytes(), key.as_bytes(), 2).unwrap();

    let first = authority.issue("one.example.com").unwrap();
    let again = authority.issue("two.example.com").unwrap();
    authority.issue("one.example.org").unwrap();
    authority.issue("one.example.net").unwrap();

    assert!(std::sync::Arc::ptr_eq(&first, &again));
    assert_eq!(authority.cache_len(), 2);
}

#[test]
fn authority_reissues_expired_cache_entries() {
    let (certificate, key) = test_ca();
    let authority = TlsAuthority::from_pem(certificate.as_bytes(), key.as_bytes(), 4).unwrap();

    let first = authority.issue("www.example.com").unwrap();
    authority.expire_for_test("*.example.com");
    let second = authority.issue("api.example.com").unwrap();

    assert!(!std::sync::Arc::ptr_eq(&first, &second));
}

fn test_ca() -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["Agora Sandbox Test CA".to_string()]).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params.self_signed(&key).unwrap();
    (certificate.pem(), key.serialize_pem())
}

fn subject_alt_names(der: &[u8]) -> Vec<String> {
    let (_, certificate) = X509Certificate::from_der(der).unwrap();
    certificate
        .subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(value) => Some((*value).to_string()),
            GeneralName::IPAddress(value) if value.len() == 4 => {
                Some(IpAddr::V4(Ipv4Addr::new(value[0], value[1], value[2], value[3])).to_string())
            }
            _ => None,
        })
        .collect()
}

fn ca_der(certificate: &str) -> Vec<u8> {
    CertificateDer::pem_slice_iter(certificate.as_bytes())
        .next()
        .unwrap()
        .unwrap()
        .to_vec()
}
