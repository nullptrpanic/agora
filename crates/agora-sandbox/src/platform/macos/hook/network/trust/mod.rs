#![cfg(target_os = "macos")]

use super::super::config;
use super::super::dyld::{dyld_interpose, function_from_interpose};
use base64::Engine;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

type CfType = *const c_void;
type CfArray = *const c_void;
type CfDictionary = *const c_void;
type SecCertificate = *const c_void;
type SecTrust = *const c_void;
type OsStatus = i32;
type CfBoolean = u8;

const ERR_SEC_SUCCESS: OsStatus = 0;
const ERR_SEC_DECODE: OsStatus = -26275;
const SEC_TRUST_RESULT_FATAL_FAILURE: u32 = 6;

type SecTrustCreateWithCertificatesFn =
    unsafe extern "C" fn(CfType, CfType, *mut SecTrust) -> OsStatus;
type SecTrustEvaluateFn = unsafe extern "C" fn(SecTrust, *mut u32) -> OsStatus;
type SecTrustEvaluateAsyncFn =
    unsafe extern "C" fn(SecTrust, *mut c_void, *const c_void) -> OsStatus;
type SecTrustEvaluateWithErrorFn = unsafe extern "C" fn(SecTrust, *mut CfType) -> bool;
type SecTrustEvaluateAsyncWithErrorFn =
    unsafe extern "C" fn(SecTrust, *mut c_void, *const c_void) -> OsStatus;

#[repr(C)]
struct CfArrayCallbacks {
    version: isize,
    retain: Option<unsafe extern "C" fn(CfType, CfType) -> CfType>,
    release: Option<unsafe extern "C" fn(CfType, CfType)>,
    copy_description: Option<unsafe extern "C" fn(CfType) -> CfType>,
    equal: Option<unsafe extern "C" fn(CfType, CfType) -> CfBoolean>,
}

pub(super) struct TrustAnchor {
    der: Vec<u8>,
}

struct TrustAnchors(Vec<TrustAnchor>);

enum TrustRuntime {
    Disabled,
    Ready(TrustAnchors),
    Invalid,
}

impl TrustRuntime {
    fn global() -> &'static Self {
        static RUNTIME: OnceLock<TrustRuntime> = OnceLock::new();
        RUNTIME.get_or_init(|| {
            Self::from_encoded_der(
                config::global().and_then(config::HookConfig::tls_trust_anchor_der),
            )
        })
    }

    fn from_encoded_der(der: Option<&str>) -> Self {
        let Some(der) = der else {
            return Self::Disabled;
        };
        let anchors = der
            .split(',')
            .map(|encoded| {
                if encoded.is_empty() {
                    return Err(());
                }
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|_| ())
                    .and_then(|der| TrustAnchor::from_der(&der))
            })
            .collect::<Result<Vec<_>, _>>();
        match anchors {
            Ok(anchors) if !anchors.is_empty() => Self::Ready(TrustAnchors(anchors)),
            _ => Self::Invalid,
        }
    }

    unsafe fn prepare(&self, trust: SecTrust) -> Result<(), ()> {
        match self {
            Self::Disabled => Ok(()),
            Self::Ready(anchors) => {
                if unsafe { has_ssl_policy(trust) }? {
                    unsafe { anchors.inject(trust) }
                } else {
                    Ok(())
                }
            }
            Self::Invalid => {
                if unsafe { has_ssl_policy(trust) }? {
                    Err(())
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl TrustAnchor {
    pub(super) fn from_der(der: &[u8]) -> Result<Self, ()> {
        let anchor = Self { der: der.to_vec() };
        let certificate = anchor.certificate()?;
        unsafe { cf_release(certificate) };
        Ok(anchor)
    }

    #[cfg(test)]
    pub(super) unsafe fn inject(&self, trust: SecTrust) -> Result<(), ()> {
        unsafe { TrustAnchors::from_ref(self).inject(trust) }
    }

    fn certificate(&self) -> Result<SecCertificate, ()> {
        let length = self.der.len().try_into().map_err(|_| ())?;
        let data = unsafe { cf_data_create(std::ptr::null(), self.der.as_ptr(), length) };
        if data.is_null() {
            return Err(());
        }
        let certificate = unsafe { sec_certificate_create_with_data(std::ptr::null(), data) };
        unsafe { cf_release(data) };
        (!certificate.is_null()).then_some(certificate).ok_or(())
    }
}

impl TrustAnchors {
    #[cfg(test)]
    fn from_ref(anchor: &TrustAnchor) -> Self {
        Self(vec![TrustAnchor {
            der: anchor.der.clone(),
        }])
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }

    unsafe fn inject(&self, trust: SecTrust) -> Result<(), ()> {
        if trust.is_null() {
            return Err(());
        }
        let mut custom = std::ptr::null();
        if unsafe { sec_trust_copy_custom_anchor_certificates(trust, &mut custom) }
            != ERR_SEC_SUCCESS
        {
            return Err(());
        }
        if !custom.is_null() {
            unsafe { cf_release(custom) };
            return Ok(());
        }

        let mut values = Vec::with_capacity(self.0.len());
        for anchor in &self.0 {
            match anchor.certificate() {
                Ok(certificate) => values.push(certificate),
                Err(()) => {
                    for certificate in values {
                        unsafe { cf_release(certificate) };
                    }
                    return Err(());
                }
            }
        }
        let anchors = unsafe {
            cf_array_create(
                std::ptr::null(),
                values.as_ptr(),
                values.len().try_into().map_err(|_| ())?,
                std::ptr::addr_of!(kCFTypeArrayCallBacks),
            )
        };
        if anchors.is_null() {
            for certificate in values {
                unsafe { cf_release(certificate) };
            }
            return Err(());
        }
        let set = unsafe { sec_trust_set_anchor_certificates(trust, anchors) };
        unsafe { cf_release(anchors) };
        for certificate in values {
            unsafe { cf_release(certificate) };
        }
        if set != ERR_SEC_SUCCESS {
            return Err(());
        }
        (unsafe { sec_trust_set_anchor_certificates_only(trust, 0) } == ERR_SEC_SUCCESS)
            .then_some(())
            .ok_or(())
    }
}

pub(super) unsafe fn has_ssl_policy(trust: SecTrust) -> Result<bool, ()> {
    if trust.is_null() {
        return Err(());
    }
    let mut policies = std::ptr::null();
    if unsafe { sec_trust_copy_policies(trust, &mut policies) } != ERR_SEC_SUCCESS
        || policies.is_null()
    {
        return Err(());
    }
    let result = unsafe {
        let count = cf_array_get_count(policies);
        (0..count).any(|index| policy_is_ssl(cf_array_get_value_at_index(policies, index)))
    };
    unsafe { cf_release(policies) };
    Ok(result)
}

unsafe fn policy_is_ssl(policy: CfType) -> bool {
    if policy.is_null() {
        return false;
    }
    let properties = unsafe { sec_policy_copy_properties(policy) };
    if properties.is_null() {
        return false;
    }
    let oid = unsafe { cf_dictionary_get_value(properties, kSecPolicyOid) };
    let result = !oid.is_null() && unsafe { cf_equal(oid, kSecPolicyAppleSSL) } != 0;
    unsafe { cf_release(properties) };
    result
}

fn prepare(trust: SecTrust) -> bool {
    let _signals = super::super::SignalMaskGuard::block_or_abort();
    catch_unwind(AssertUnwindSafe(|| unsafe {
        TrustRuntime::global().prepare(trust)
    }))
    .is_ok_and(|result| result.is_ok())
}

unsafe fn create_with_certificates(
    original: Option<SecTrustCreateWithCertificatesFn>,
    certificates: CfType,
    policies: CfType,
    trust: *mut SecTrust,
    prepare_trust: impl FnOnce(SecTrust) -> bool,
) -> OsStatus {
    let Some(original) = original else {
        return ERR_SEC_DECODE;
    };
    let status = unsafe { original(certificates, policies, trust) };
    if status != ERR_SEC_SUCCESS || trust.is_null() || unsafe { *trust }.is_null() {
        return status;
    }
    if prepare_trust(unsafe { *trust }) {
        status
    } else {
        unsafe { cf_release(*trust) };
        unsafe { *trust = std::ptr::null() };
        ERR_SEC_DECODE
    }
}

unsafe fn evaluate(
    original: Option<SecTrustEvaluateFn>,
    trust: SecTrust,
    result: *mut u32,
    prepare_trust: impl FnOnce(SecTrust) -> bool,
) -> OsStatus {
    let Some(original) = original else {
        return ERR_SEC_DECODE;
    };
    if !prepare_trust(trust) {
        if !result.is_null() {
            unsafe { *result = SEC_TRUST_RESULT_FATAL_FAILURE };
        }
        return ERR_SEC_DECODE;
    }
    unsafe { original(trust, result) }
}

unsafe fn evaluate_async(
    original: Option<SecTrustEvaluateAsyncFn>,
    trust: SecTrust,
    queue: *mut c_void,
    callback: *const c_void,
    prepare_trust: impl FnOnce(SecTrust) -> bool,
) -> OsStatus {
    let Some(original) = original else {
        return ERR_SEC_DECODE;
    };
    if !prepare_trust(trust) {
        return ERR_SEC_DECODE;
    }
    unsafe { original(trust, queue, callback) }
}

unsafe fn evaluate_with_error(
    original: Option<SecTrustEvaluateWithErrorFn>,
    trust: SecTrust,
    error: *mut CfType,
    prepare_trust: impl FnOnce(SecTrust) -> bool,
) -> bool {
    let Some(original) = original else {
        return false;
    };
    if !prepare_trust(trust) {
        if !error.is_null() {
            unsafe { *error = std::ptr::null() };
        }
        return false;
    }
    unsafe { original(trust, error) }
}

unsafe fn evaluate_async_with_error(
    original: Option<SecTrustEvaluateAsyncWithErrorFn>,
    trust: SecTrust,
    queue: *mut c_void,
    callback: *const c_void,
    prepare_trust: impl FnOnce(SecTrust) -> bool,
) -> OsStatus {
    let Some(original) = original else {
        return ERR_SEC_DECODE;
    };
    if !prepare_trust(trust) {
        return ERR_SEC_DECODE;
    }
    unsafe { original(trust, queue, callback) }
}

macro_rules! trust_hook {
    ($symbol:literal, $name:ident($($argument:ident: $type:ty),*) -> $output:ty, $delegate:expr) => {
        #[unsafe(export_name = $symbol)]
        unsafe extern "C" fn $name($($argument: $type),*) -> $output {
            unsafe { $delegate }
        }
    };
}

trust_hook!(
    "agora_sandbox_sec_trust_create_with_certificates",
    hook_create(certs: CfType, policies: CfType, trust: *mut SecTrust) -> OsStatus,
    create_with_certificates(original_create(), certs, policies, trust, prepare)
);
trust_hook!(
    "agora_sandbox_sec_trust_evaluate",
    hook_evaluate(trust: SecTrust, result: *mut u32) -> OsStatus,
    evaluate(original_evaluate(), trust, result, prepare)
);
trust_hook!(
    "agora_sandbox_sec_trust_evaluate_async",
    hook_evaluate_async(trust: SecTrust, queue: *mut c_void, callback: *const c_void) -> OsStatus,
    evaluate_async(original_evaluate_async(), trust, queue, callback, prepare)
);
trust_hook!(
    "agora_sandbox_sec_trust_evaluate_with_error",
    hook_evaluate_with_error(trust: SecTrust, error: *mut CfType) -> bool,
    evaluate_with_error(original_evaluate_with_error(), trust, error, prepare)
);
trust_hook!(
    "agora_sandbox_sec_trust_evaluate_async_with_error",
    hook_evaluate_async_with_error(
        trust: SecTrust,
        queue: *mut c_void,
        callback: *const c_void
    ) -> OsStatus,
    evaluate_async_with_error(
        original_evaluate_async_with_error(),
        trust,
        queue,
        callback,
        prepare,
    )
);

fn original_create() -> Option<SecTrustCreateWithCertificatesFn> {
    function_from_interpose(&INTERPOSE_SEC_TRUST_CREATE_WITH_CERTIFICATES)
}

fn original_evaluate() -> Option<SecTrustEvaluateFn> {
    function_from_interpose(&INTERPOSE_SEC_TRUST_EVALUATE)
}

fn original_evaluate_async() -> Option<SecTrustEvaluateAsyncFn> {
    function_from_interpose(&INTERPOSE_SEC_TRUST_EVALUATE_ASYNC)
}

fn original_evaluate_with_error() -> Option<SecTrustEvaluateWithErrorFn> {
    function_from_interpose(&INTERPOSE_SEC_TRUST_EVALUATE_WITH_ERROR)
}

fn original_evaluate_async_with_error() -> Option<SecTrustEvaluateAsyncWithErrorFn> {
    function_from_interpose(&INTERPOSE_SEC_TRUST_EVALUATE_ASYNC_WITH_ERROR)
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CfArrayCallbacks;

    #[link_name = "CFArrayCreate"]
    fn cf_array_create(
        allocator: CfType,
        values: *const CfType,
        count: isize,
        callbacks: *const CfArrayCallbacks,
    ) -> CfArray;
    #[link_name = "CFArrayGetCount"]
    fn cf_array_get_count(array: CfArray) -> isize;
    #[link_name = "CFArrayGetValueAtIndex"]
    fn cf_array_get_value_at_index(array: CfArray, index: isize) -> CfType;
    #[link_name = "CFDataCreate"]
    fn cf_data_create(allocator: CfType, bytes: *const u8, length: isize) -> CfType;
    #[link_name = "CFDictionaryGetValue"]
    fn cf_dictionary_get_value(dictionary: CfDictionary, key: CfType) -> CfType;
    #[link_name = "CFEqual"]
    fn cf_equal(left: CfType, right: CfType) -> CfBoolean;
    #[link_name = "CFRelease"]
    fn cf_release(value: CfType);
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecPolicyAppleSSL: CfType;
    static kSecPolicyOid: CfType;

    #[link_name = "SecCertificateCreateWithData"]
    fn sec_certificate_create_with_data(allocator: CfType, data: CfType) -> SecCertificate;
    #[link_name = "SecPolicyCopyProperties"]
    fn sec_policy_copy_properties(policy: CfType) -> CfDictionary;
    #[link_name = "SecTrustCopyCustomAnchorCertificates"]
    fn sec_trust_copy_custom_anchor_certificates(
        trust: SecTrust,
        anchors: *mut CfArray,
    ) -> OsStatus;
    #[link_name = "SecTrustCopyPolicies"]
    fn sec_trust_copy_policies(trust: SecTrust, policies: *mut CfArray) -> OsStatus;
    #[link_name = "SecTrustSetAnchorCertificates"]
    fn sec_trust_set_anchor_certificates(trust: SecTrust, anchors: CfArray) -> OsStatus;
    #[link_name = "SecTrustSetAnchorCertificatesOnly"]
    fn sec_trust_set_anchor_certificates_only(trust: SecTrust, only: u8) -> OsStatus;

    #[link_name = "SecTrustCreateWithCertificates"]
    fn system_sec_trust_create_with_certificates(
        certificates: CfType,
        policies: CfType,
        trust: *mut SecTrust,
    ) -> OsStatus;
    #[link_name = "SecTrustEvaluate"]
    fn system_sec_trust_evaluate(trust: SecTrust, result: *mut u32) -> OsStatus;
    #[link_name = "SecTrustEvaluateAsync"]
    fn system_sec_trust_evaluate_async(
        trust: SecTrust,
        queue: *mut c_void,
        callback: *const c_void,
    ) -> OsStatus;
    #[link_name = "SecTrustEvaluateWithError"]
    fn system_sec_trust_evaluate_with_error(trust: SecTrust, error: *mut CfType) -> bool;
    #[link_name = "SecTrustEvaluateAsyncWithError"]
    fn system_sec_trust_evaluate_async_with_error(
        trust: SecTrust,
        queue: *mut c_void,
        callback: *const c_void,
    ) -> OsStatus;
}

dyld_interpose!(
    INTERPOSE_SEC_TRUST_CREATE_WITH_CERTIFICATES,
    hook_create,
    system_sec_trust_create_with_certificates
);
dyld_interpose!(
    INTERPOSE_SEC_TRUST_EVALUATE,
    hook_evaluate,
    system_sec_trust_evaluate
);
dyld_interpose!(
    INTERPOSE_SEC_TRUST_EVALUATE_ASYNC,
    hook_evaluate_async,
    system_sec_trust_evaluate_async
);
dyld_interpose!(
    INTERPOSE_SEC_TRUST_EVALUATE_WITH_ERROR,
    hook_evaluate_with_error,
    system_sec_trust_evaluate_with_error
);
dyld_interpose!(
    INTERPOSE_SEC_TRUST_EVALUATE_ASYNC_WITH_ERROR,
    hook_evaluate_async_with_error,
    system_sec_trust_evaluate_async_with_error
);

#[cfg(test)]
mod tests;
