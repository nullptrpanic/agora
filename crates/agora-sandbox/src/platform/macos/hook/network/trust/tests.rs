use super::{
    ERR_SEC_DECODE, SEC_TRUST_RESULT_FATAL_FAILURE, TrustAnchor, TrustAnchors, TrustRuntime,
    create_with_certificates, evaluate, evaluate_async, evaluate_async_with_error,
    evaluate_with_error, has_ssl_policy, hook_create, hook_evaluate, hook_evaluate_with_error,
    original_create, original_evaluate, original_evaluate_async,
    original_evaluate_async_with_error, original_evaluate_with_error, policy_is_ssl, prepare,
};
use base64::Engine;
use std::ffi::{CString, c_void};

const CA_DER: &str = include_str!("../../../../../../tests/fixtures/test-ca.der.b64");
const LEAF_DER: &str = include_str!("../../../../../../tests/fixtures/test-leaf.der.b64");

type CfType = *const c_void;
type SecTrust = *const c_void;

const TEST_VERIFY_TIME: f64 = 807_235_200.0;

struct Owned(CfType);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { cf_release(self.0) };
    }
}

#[test]
fn ssl_trust_accepts_the_leaf_after_anchor_injection() {
    let trust = ssl_trust("example.test");
    assert!(!unsafe { sec_trust_evaluate_with_error(trust.0, std::ptr::null_mut()) });
    assert!(unsafe { has_ssl_policy(trust.0) }.unwrap());

    let anchor = TrustAnchor::from_der(&decode(CA_DER)).unwrap();
    unsafe { anchor.inject(trust.0) }.unwrap();

    assert!(unsafe { sec_trust_evaluate_with_error(trust.0, std::ptr::null_mut()) });
}

#[test]
fn basic_x509_trust_is_not_an_ssl_policy() {
    let certificate = certificate(&decode(LEAF_DER));
    let policy = Owned(unsafe { sec_policy_create_basic_x509() });
    let trust = trust(certificate.0, policy.0);

    assert!(!unsafe { has_ssl_policy(trust.0) }.unwrap());
}

#[test]
fn malformed_anchor_is_rejected() {
    assert!(TrustAnchor::from_der(b"not a certificate").is_err());
    assert!(matches!(
        TrustRuntime::from_encoded_der(Some("")),
        TrustRuntime::Invalid
    ));
}

#[test]
fn anchor_injection_releases_certificates_when_a_later_anchor_is_invalid() {
    let trust = ssl_trust("example.test");
    let anchors = TrustAnchors(vec![
        TrustAnchor::from_der(&decode(CA_DER)).unwrap(),
        TrustAnchor {
            der: b"not a certificate".to_vec(),
        },
    ]);

    assert!(unsafe { anchors.inject(trust.0) }.is_err());
}

#[test]
fn all_supported_sec_trust_entry_points_are_interposed() {
    assert!(original_create().is_some());
    assert!(original_evaluate().is_some());
    assert!(original_evaluate_async().is_some());
    assert!(original_evaluate_with_error().is_some());
    assert!(original_evaluate_async_with_error().is_some());
}

#[test]
fn trust_runtime_applies_only_valid_ssl_anchors() {
    let ssl = ssl_trust("example.test");
    let basic = basic_trust();
    let anchor = TrustAnchor::from_der(&decode(CA_DER)).unwrap();
    let ready = TrustRuntime::Ready(TrustAnchors(vec![anchor]));

    assert!(matches!(
        TrustRuntime::from_encoded_der(None),
        TrustRuntime::Disabled
    ));
    assert!(matches!(
        TrustRuntime::from_encoded_der(Some("invalid base64")),
        TrustRuntime::Invalid
    ));
    let encoded = base64::engine::general_purpose::STANDARD.encode(decode(CA_DER));
    assert!(matches!(
        TrustRuntime::from_encoded_der(Some(&encoded)),
        TrustRuntime::Ready(_)
    ));
    assert!(unsafe { TrustRuntime::Disabled.prepare(std::ptr::null()) }.is_ok());
    assert!(unsafe { ready.prepare(basic.0) }.is_ok());
    assert!(unsafe { TrustRuntime::Invalid.prepare(basic.0) }.is_ok());
    assert!(unsafe { TrustRuntime::Invalid.prepare(ssl.0) }.is_err());
    assert!(unsafe { ready.prepare(ssl.0) }.is_ok());
    assert!(unsafe { ready.prepare(ssl.0) }.is_ok());
    assert!(unsafe { has_ssl_policy(std::ptr::null()) }.is_err());
    assert!(!unsafe { policy_is_ssl(std::ptr::null()) });
    assert!(
        unsafe {
            TrustAnchor::from_der(&decode(CA_DER))
                .unwrap()
                .inject(std::ptr::null())
        }
        .is_err()
    );
    assert!(prepare(std::ptr::null()));
}

#[test]
fn trust_runtime_accepts_multiple_encoded_anchors() {
    let encoded = base64::engine::general_purpose::STANDARD.encode(decode(CA_DER));
    let encoded = format!("{encoded},{encoded}");

    let TrustRuntime::Ready(anchors) = TrustRuntime::from_encoded_der(Some(&encoded)) else {
        panic!("expected valid trust anchors");
    };

    assert_eq!(anchors.len(), 2);
    let ssl = ssl_trust("example.test");
    assert!(unsafe { TrustRuntime::Ready(anchors).prepare(ssl.0) }.is_ok());
}

#[test]
fn synchronous_interposers_delegate_to_security_framework() {
    let certificate = certificate(&decode(LEAF_DER));
    let hostname = CString::new("example.test").unwrap();
    let hostname = Owned(unsafe {
        cf_string_create_with_c_string(std::ptr::null(), hostname.as_ptr(), 0x0800_0100)
    });
    let policy = Owned(unsafe { sec_policy_create_ssl(1, hostname.0) });
    let mut trust: SecTrust = std::ptr::null();

    assert_eq!(
        unsafe { hook_create(certificate.0, policy.0, &mut trust) },
        0
    );
    let mut rejected: SecTrust = std::ptr::null();
    assert_eq!(
        unsafe {
            create_with_certificates(
                original_create(),
                certificate.0,
                policy.0,
                &mut rejected,
                |_| false,
            )
        },
        ERR_SEC_DECODE
    );
    assert!(rejected.is_null());
    assert!(!trust.is_null());
    let trust = Owned(trust);
    set_test_verify_date(trust.0);

    assert!(!unsafe { hook_evaluate_with_error(trust.0, std::ptr::null_mut()) });

    let anchor = TrustAnchor::from_der(&decode(CA_DER)).unwrap();
    unsafe { anchor.inject(trust.0) }.unwrap();
    assert!(unsafe { hook_evaluate_with_error(trust.0, std::ptr::null_mut()) });

    let mut result = 0;
    assert_eq!(unsafe { hook_evaluate(trust.0, &mut result) }, 0);
    assert_ne!(result, 6);
}

#[test]
fn trust_entry_point_helpers_fail_closed_and_delegate() {
    let mut trust = std::ptr::null();
    assert_eq!(
        unsafe {
            create_with_certificates(None, std::ptr::null(), std::ptr::null(), &mut trust, |_| {
                true
            })
        },
        ERR_SEC_DECODE
    );
    assert_eq!(
        unsafe {
            create_with_certificates(
                Some(fake_create_failure),
                std::ptr::null(),
                std::ptr::null(),
                &mut trust,
                |_| true,
            )
        },
        -50
    );

    let mut result = 0;
    assert_eq!(
        unsafe { evaluate(None, std::ptr::null(), &mut result, |_| true) },
        ERR_SEC_DECODE
    );
    assert_eq!(
        unsafe {
            evaluate(Some(fake_evaluate), std::ptr::null(), &mut result, |_| {
                false
            })
        },
        ERR_SEC_DECODE
    );
    assert_eq!(result, SEC_TRUST_RESULT_FATAL_FAILURE);
    assert_eq!(
        unsafe { evaluate(Some(fake_evaluate), std::ptr::null(), &mut result, |_| true,) },
        0
    );
    assert_eq!(result, 1);

    assert_eq!(
        unsafe {
            evaluate_async(
                None,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                |_| true,
            )
        },
        ERR_SEC_DECODE
    );
    assert_eq!(
        unsafe {
            evaluate_async(
                Some(fake_evaluate_async),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                |_| false,
            )
        },
        ERR_SEC_DECODE
    );
    assert_eq!(
        unsafe {
            evaluate_async(
                Some(fake_evaluate_async),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                |_| true,
            )
        },
        41
    );

    let mut error = 1_usize as CfType;
    assert!(!unsafe { evaluate_with_error(None, std::ptr::null(), &mut error, |_| true) });
    assert!(!unsafe {
        evaluate_with_error(
            Some(fake_evaluate_with_error),
            std::ptr::null(),
            &mut error,
            |_| false,
        )
    });
    assert!(error.is_null());
    assert!(unsafe {
        evaluate_with_error(
            Some(fake_evaluate_with_error),
            std::ptr::null(),
            &mut error,
            |_| true,
        )
    });

    assert_eq!(
        unsafe {
            evaluate_async_with_error(
                None,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                |_| true,
            )
        },
        ERR_SEC_DECODE
    );
    assert_eq!(
        unsafe {
            evaluate_async_with_error(
                Some(fake_evaluate_async_with_error),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                |_| false,
            )
        },
        ERR_SEC_DECODE
    );
    assert_eq!(
        unsafe {
            evaluate_async_with_error(
                Some(fake_evaluate_async_with_error),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                |_| true,
            )
        },
        42
    );
}

unsafe extern "C" fn fake_create_failure(
    _certificates: CfType,
    _policies: CfType,
    _trust: *mut SecTrust,
) -> i32 {
    -50
}

unsafe extern "C" fn fake_evaluate(_trust: SecTrust, result: *mut u32) -> i32 {
    if !result.is_null() {
        unsafe { *result = 1 };
    }
    0
}

unsafe extern "C" fn fake_evaluate_async(
    _trust: SecTrust,
    _queue: *mut c_void,
    _callback: *const c_void,
) -> i32 {
    41
}

unsafe extern "C" fn fake_evaluate_with_error(_trust: SecTrust, _error: *mut CfType) -> bool {
    true
}

unsafe extern "C" fn fake_evaluate_async_with_error(
    _trust: SecTrust,
    _queue: *mut c_void,
    _callback: *const c_void,
) -> i32 {
    42
}

fn ssl_trust(host: &str) -> Owned {
    let certificate = certificate(&decode(LEAF_DER));
    let host = CString::new(host).unwrap();
    let host = Owned(unsafe {
        cf_string_create_with_c_string(std::ptr::null(), host.as_ptr(), 0x0800_0100)
    });
    let policy = Owned(unsafe { sec_policy_create_ssl(1, host.0) });
    trust(certificate.0, policy.0)
}

fn basic_trust() -> Owned {
    let certificate = certificate(&decode(LEAF_DER));
    let policy = Owned(unsafe { sec_policy_create_basic_x509() });
    trust(certificate.0, policy.0)
}

fn certificate(der: &[u8]) -> Owned {
    let data = Owned(unsafe {
        cf_data_create(
            std::ptr::null(),
            der.as_ptr(),
            der.len().try_into().unwrap(),
        )
    });
    Owned(unsafe { sec_certificate_create_with_data(std::ptr::null(), data.0) })
}

fn trust(certificate: CfType, policy: CfType) -> Owned {
    let mut trust: SecTrust = std::ptr::null();
    assert_eq!(
        unsafe { sec_trust_create_with_certificates(certificate, policy, &mut trust) },
        0
    );
    assert!(!trust.is_null());
    set_test_verify_date(trust);
    Owned(trust)
}

fn set_test_verify_date(trust: SecTrust) {
    let date = Owned(unsafe { cf_date_create(std::ptr::null(), TEST_VERIFY_TIME) });
    assert!(!date.0.is_null());
    assert_eq!(unsafe { sec_trust_set_verify_date(trust, date.0) }, 0);
}

fn decode(value: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .unwrap()
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: CfType, bytes: *const u8, length: isize) -> CfType;
    fn CFDateCreate(allocator: CfType, absolute_time: f64) -> CfType;
    fn CFStringCreateWithCString(
        allocator: CfType,
        bytes: *const libc::c_char,
        encoding: u32,
    ) -> CfType;
    fn CFRelease(value: CfType);
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCertificateCreateWithData(allocator: CfType, data: CfType) -> CfType;
    fn SecPolicyCreateBasicX509() -> CfType;
    fn SecPolicyCreateSSL(server: u8, hostname: CfType) -> CfType;
    fn SecTrustCreateWithCertificates(
        certificates: CfType,
        policies: CfType,
        trust: *mut SecTrust,
    ) -> i32;
    fn SecTrustEvaluateWithError(trust: SecTrust, error: *mut CfType) -> bool;
    fn SecTrustSetVerifyDate(trust: SecTrust, date: CfType) -> i32;
}

unsafe fn cf_data_create(allocator: CfType, bytes: *const u8, length: isize) -> CfType {
    unsafe { CFDataCreate(allocator, bytes, length) }
}

unsafe fn cf_date_create(allocator: CfType, absolute_time: f64) -> CfType {
    unsafe { CFDateCreate(allocator, absolute_time) }
}

unsafe fn cf_string_create_with_c_string(
    allocator: CfType,
    bytes: *const libc::c_char,
    encoding: u32,
) -> CfType {
    unsafe { CFStringCreateWithCString(allocator, bytes, encoding) }
}

unsafe fn cf_release(value: CfType) {
    unsafe { CFRelease(value) };
}

unsafe fn sec_certificate_create_with_data(allocator: CfType, data: CfType) -> CfType {
    unsafe { SecCertificateCreateWithData(allocator, data) }
}

unsafe fn sec_policy_create_basic_x509() -> CfType {
    unsafe { SecPolicyCreateBasicX509() }
}

unsafe fn sec_policy_create_ssl(server: u8, hostname: CfType) -> CfType {
    unsafe { SecPolicyCreateSSL(server, hostname) }
}

unsafe fn sec_trust_create_with_certificates(
    certificates: CfType,
    policies: CfType,
    trust: *mut SecTrust,
) -> i32 {
    unsafe { SecTrustCreateWithCertificates(certificates, policies, trust) }
}

unsafe fn sec_trust_evaluate_with_error(trust: SecTrust, error: *mut CfType) -> bool {
    unsafe { SecTrustEvaluateWithError(trust, error) }
}

unsafe fn sec_trust_set_verify_date(trust: SecTrust, date: CfType) -> i32 {
    unsafe { SecTrustSetVerifyDate(trust, date) }
}
