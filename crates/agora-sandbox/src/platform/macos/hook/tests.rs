use super::config::HookConfig;
use super::network::{ProcessContext, RawSocketAddress, socket_addr_from_raw};
use crate::filesystem::FileCipher;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

pub(crate) struct SignalMaskProbe {
    previous: libc::sigset_t,
    signal: libc::c_int,
}

impl SignalMaskProbe {
    pub(crate) fn unblocked(signal: libc::c_int) -> Self {
        let mut selected = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        unsafe {
            libc::sigemptyset(selected.as_mut_ptr());
            libc::sigaddset(selected.as_mut_ptr(), signal);
            let selected = selected.assume_init();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &selected, previous.as_mut_ptr()),
                0
            );
            Self {
                previous: previous.assume_init(),
                signal,
            }
        }
    }

    pub(crate) fn is_blocked(&self) -> bool {
        Self::signal_is_blocked(self.signal)
    }

    pub(crate) fn signal_is_blocked(signal: libc::c_int) -> bool {
        let mut current = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        unsafe {
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), current.as_mut_ptr(),),
                0
            );
            libc::sigismember(&current.assume_init(), signal) == 1
        }
    }
}

impl Drop for SignalMaskProbe {
    fn drop(&mut self) {
        assert_eq!(
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
            },
            0
        );
    }
}

#[test]
fn ipv4_socket_address_round_trips_through_raw_storage() {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 443);
    let raw = RawSocketAddress::new(address);

    let decoded = unsafe { socket_addr_from_raw(raw.as_ptr(), raw.len()) };

    assert_eq!(decoded, Some(address));
}

#[test]
fn ipv6_socket_address_round_trips_scope_and_flow_information() {
    let address = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 8443, 12, 7));
    let raw = RawSocketAddress::new(address);

    let decoded = unsafe { socket_addr_from_raw(raw.as_ptr(), raw.len()) };

    assert_eq!(decoded, Some(address));
}

#[test]
fn hook_configuration_requires_all_runtime_values() {
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain"),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
    ]);
    let config = HookConfig::from_getter(|key| values.get(key).map(ToString::to_string)).unwrap();

    assert_eq!(config.tls_trust_anchor_der(), None);
    assert_eq!(config.audit_control(), "127.0.0.1:41003".parse().unwrap());
    assert_eq!(config.audit_token(), "audit-token");

    assert_eq!(
        config.proxy_for(SocketAddr::from(([203, 0, 113, 10], 443))),
        SocketAddr::from(([127, 0, 0, 1], 41000))
    );
    assert_eq!(
        config.proxy_for(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443))),
        "[::1]:41001".parse().unwrap()
    );
    assert!(config.is_internal("127.0.0.1:41000".parse().unwrap()));
    assert!(config.is_internal("[::1]:41001".parse().unwrap()));
    assert!(config.is_internal("127.0.0.1:41002".parse().unwrap()));
    assert!(config.is_internal("127.0.0.1:41003".parse().unwrap()));

    let error = HookConfig::from_getter(|key| {
        (key != "AGORA_SANDBOX_TOKEN")
            .then(|| values.get(key).map(ToString::to_string))
            .flatten()
    })
    .unwrap_err();
    assert!(error.contains("AGORA_SANDBOX_TOKEN"));
}

#[test]
fn hook_configuration_propagates_an_optional_tls_trust_anchor() {
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain"),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
        ("AGORA_SANDBOX_TLS_TRUST_ANCHOR_DER", "Y2VydGlmaWNhdGU="),
        ("AGORA_SANDBOX_TLS_TRUST_BUNDLE", "/tmp/agora-ca.pem"),
    ]);

    let config = HookConfig::from_getter(|key| values.get(key).map(ToString::to_string)).unwrap();

    assert_eq!(config.tls_trust_anchor_der(), Some("Y2VydGlmaWNhdGU="));
    assert_eq!(config.tls_trust_bundle(), Some("/tmp/agora-ca.pem"));
    assert!(config.child_environment().contains(&(
        "AGORA_SANDBOX_TLS_TRUST_ANCHOR_DER",
        "Y2VydGlmaWNhdGU=".to_string()
    )));
    for key in [
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "REQUESTS_CA_BUNDLE",
        "NODE_EXTRA_CA_CERTS",
        "GIT_SSL_CAINFO",
    ] {
        assert!(
            config
                .child_environment()
                .contains(&(key, "/tmp/agora-ca.pem".to_string())),
            "missing {key}"
        );
    }
}

#[test]
fn encrypted_hook_configuration_reuses_derived_cipher_key_material() {
    let encoded_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "encrypted"),
        ("AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY", encoded_key),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
    ]);

    let config = HookConfig::from_getter(|key| values.get(key).map(ToString::to_string)).unwrap();

    assert_eq!(
        config.filesystem_cipher().unwrap().key_id(),
        FileCipher::from_key(&[0; 32]).unwrap().key_id()
    );
    assert!(config.child_environment().contains(&(
        "AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY",
        encoded_key.to_string()
    )));
}

#[test]
fn hook_configuration_exposes_local_broker_and_runtime_accessors() {
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain"),
        ("AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL", "/tmp/local.sock"),
        ("AGORA_SANDBOX_LOCAL_FILESYSTEM_TOKEN", "local-token"),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
    ]);

    let config = HookConfig::from_getter(|key| values.get(key).map(ToString::to_string)).unwrap();

    assert_eq!(config.token(), "token");
    assert_eq!(
        config.execution_control(),
        "127.0.0.1:41002".parse().unwrap()
    );
    assert_eq!(config.execution_token(), "execution-token");
    assert_eq!(config.hook_libraries(), "/tmp/hook.dylib");
    assert_eq!(config.filesystem_root(), "/tmp/agora-fs");
    assert_eq!(
        config.local_filesystem(),
        Some(("/tmp/local.sock", "local-token"))
    );
    assert_eq!(config.remote_filesystem(), None);
    assert_eq!(config.remote_current_directory(), None);
    assert_eq!(config.trace().encode(), "trace-root");
    assert!(!config.is_internal("127.0.0.1:42000".parse().unwrap()));
    for (key, value) in [
        ("AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL", "/tmp/local.sock"),
        ("AGORA_SANDBOX_LOCAL_FILESYSTEM_TOKEN", "local-token"),
    ] {
        assert!(
            config
                .child_environment()
                .contains(&(key, value.to_string()))
        );
    }
}

#[test]
fn hook_configuration_validates_and_propagates_remote_broker_values() {
    let routes = r#"[{"root":0,"logical_root":"/remote"}]"#;
    let values = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain"),
        ("AGORA_SANDBOX_REMOTE_CONTROL", "/tmp/remote.sock"),
        ("AGORA_SANDBOX_REMOTE_TOKEN", "remote-token"),
        ("AGORA_SANDBOX_REMOTE_ROOTS", routes),
        ("AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY", "/remote/docs"),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
    ]);

    let config = HookConfig::from_getter(|key| values.get(key).map(ToString::to_string)).unwrap();

    assert_eq!(
        config.remote_filesystem(),
        Some(("/tmp/remote.sock", "remote-token", routes))
    );
    assert_eq!(
        config.remote_current_directory(),
        Some(std::path::Path::new("/remote/docs"))
    );
    for (key, value) in [
        ("AGORA_SANDBOX_REMOTE_CONTROL", "/tmp/remote.sock"),
        ("AGORA_SANDBOX_REMOTE_TOKEN", "remote-token"),
        ("AGORA_SANDBOX_REMOTE_ROOTS", routes),
    ] {
        assert!(
            config
                .child_environment()
                .contains(&(key, value.to_string()))
        );
    }

    let partial = HookConfig::from_getter(|key| {
        (key != "AGORA_SANDBOX_REMOTE_TOKEN")
            .then(|| values.get(key).map(ToString::to_string))
            .flatten()
    });
    assert!(partial.unwrap_err().contains("remote filesystem"));

    let without_remote = HookConfig::from_getter(|key| {
        if key == "AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY" {
            Some("/remote/docs".to_string())
        } else if [
            "AGORA_SANDBOX_REMOTE_CONTROL",
            "AGORA_SANDBOX_REMOTE_TOKEN",
            "AGORA_SANDBOX_REMOTE_ROOTS",
        ]
        .contains(&key)
        {
            None
        } else {
            values.get(key).map(ToString::to_string)
        }
    });
    assert!(
        without_remote
            .unwrap_err()
            .contains("requires a remote filesystem")
    );

    let relative = HookConfig::from_getter(|key| {
        if key == "AGORA_SANDBOX_REMOTE_CURRENT_DIRECTORY" {
            Some("relative".to_string())
        } else {
            values.get(key).map(ToString::to_string)
        }
    });
    assert!(relative.unwrap_err().contains("must be an absolute path"));
}

#[test]
fn process_context_uses_the_current_process_for_each_connection() {
    let context = ProcessContext::new("/tmp/client".to_string());

    let (parent_id, parent) = context.snapshot_for(101, 100);
    let (child_id, child) = context.snapshot_for(202, 101);

    assert_eq!(parent.pid, 101);
    assert_eq!(parent.ppid, 100);
    assert_eq!(child.pid, 202);
    assert_eq!(child.ppid, 101);
    assert_eq!(child.executable, "/tmp/client");
    assert_ne!(parent_id, child_id);
}

#[test]
fn hook_configuration_rejects_invalid_or_non_loopback_proxy_addresses() {
    let valid = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain"),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
    ]);
    let parse = |overrides: &[(&str, &str)]| {
        HookConfig::from_getter(|key| {
            overrides
                .iter()
                .find_map(|(name, value)| (*name == key).then(|| (*value).to_string()))
                .or_else(|| valid.get(key).map(ToString::to_string))
        })
    };

    assert!(
        parse(&[("AGORA_SANDBOX_TOKEN", "")])
            .unwrap_err()
            .contains("TOKEN")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_PROXY_IPV4", "invalid")])
            .unwrap_err()
            .contains("invalid AGORA_SANDBOX_PROXY_IPV4")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_PROXY_IPV6", "invalid")])
            .unwrap_err()
            .contains("invalid AGORA_SANDBOX_PROXY_IPV6")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_EXECUTION_CONTROL", "invalid")])
            .unwrap_err()
            .contains("invalid AGORA_SANDBOX_EXECUTION_CONTROL")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_EXECUTION_TOKEN", "")])
            .unwrap_err()
            .contains("AGORA_SANDBOX_EXECUTION_TOKEN")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_AUDIT_CONTROL", "invalid")])
            .unwrap_err()
            .contains("invalid AGORA_SANDBOX_AUDIT_CONTROL")
    );
    for key in [
        "AGORA_SANDBOX_AUDIT_TOKEN",
        "AGORA_SANDBOX_HOOK_LIBRARIES",
        "AGORA_SANDBOX_FILESYSTEM_ROOT",
        "AGORA_SANDBOX_FILESYSTEM_MODE",
    ] {
        assert!(parse(&[(key, "")]).unwrap_err().contains(key));
    }
    assert!(
        parse(&[("AGORA_SANDBOX_TRACE_ID", "invalid trace")])
            .unwrap_err()
            .contains("invalid AGORA_SANDBOX_TRACE_ID")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_PROXY_IPV4", "203.0.113.1:80")])
            .unwrap_err()
            .contains("IPv4 loopback")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_PROXY_IPV4", "[::1]:80")])
            .unwrap_err()
            .contains("IPv4 loopback")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_PROXY_IPV6", "[2001:db8::1]:80")])
            .unwrap_err()
            .contains("IPv6 loopback")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_PROXY_IPV6", "127.0.0.1:80")])
            .unwrap_err()
            .contains("IPv6 loopback")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_EXECUTION_CONTROL", "[::1]:80")])
            .unwrap_err()
            .contains("IPv4 loopback")
    );
    assert!(
        parse(&[("AGORA_SANDBOX_AUDIT_CONTROL", "[::1]:80")])
            .unwrap_err()
            .contains("IPv4 loopback")
    );
}

#[test]
fn hook_configuration_rejects_inconsistent_filesystem_and_broker_values() {
    let valid = HashMap::from([
        ("AGORA_SANDBOX_TOKEN", "token"),
        ("AGORA_SANDBOX_PROXY_IPV4", "127.0.0.1:41000"),
        ("AGORA_SANDBOX_PROXY_IPV6", "[::1]:41001"),
        ("AGORA_SANDBOX_EXECUTION_CONTROL", "127.0.0.1:41002"),
        ("AGORA_SANDBOX_EXECUTION_TOKEN", "execution-token"),
        ("AGORA_SANDBOX_AUDIT_CONTROL", "127.0.0.1:41003"),
        ("AGORA_SANDBOX_AUDIT_TOKEN", "audit-token"),
        ("AGORA_SANDBOX_HOOK_LIBRARIES", "/tmp/hook.dylib"),
        ("AGORA_SANDBOX_FILESYSTEM_ROOT", "/tmp/agora-fs"),
        ("AGORA_SANDBOX_FILESYSTEM_MODE", "plain"),
        ("AGORA_SANDBOX_TRACE_ID", "trace-root"),
    ]);
    let parse = |overrides: &[(&str, Option<&str>)]| {
        HookConfig::from_getter(|key| {
            overrides
                .iter()
                .find_map(|(name, value)| (*name == key).then(|| value.map(str::to_string)))
                .flatten()
                .or_else(|| {
                    (!overrides.iter().any(|(name, _)| *name == key))
                        .then(|| valid.get(key).map(ToString::to_string))
                        .flatten()
                })
        })
    };

    for (overrides, expected) in [
        (
            vec![("AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY", Some("key"))],
            "plain filesystem mode cannot include a cipher key",
        ),
        (
            vec![("AGORA_SANDBOX_FILESYSTEM_MODE", Some("encrypted"))],
            "encrypted filesystem mode requires a cipher key",
        ),
        (
            vec![("AGORA_SANDBOX_FILESYSTEM_MODE", Some("unknown"))],
            "invalid AGORA_SANDBOX_FILESYSTEM_MODE",
        ),
        (
            vec![
                ("AGORA_SANDBOX_FILESYSTEM_MODE", Some("encrypted")),
                ("AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY", Some("%%%")),
            ],
            "invalid AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY",
        ),
        (
            vec![
                ("AGORA_SANDBOX_FILESYSTEM_MODE", Some("encrypted")),
                ("AGORA_SANDBOX_FILESYSTEM_CIPHER_KEY", Some("AA==")),
            ],
            "invalid encrypted filesystem configuration",
        ),
        (
            vec![("AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL", Some("relative"))],
            "local filesystem requires control and token together",
        ),
        (
            vec![
                ("AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL", Some("relative")),
                ("AGORA_SANDBOX_LOCAL_FILESYSTEM_TOKEN", Some("token")),
            ],
            "AGORA_SANDBOX_LOCAL_FILESYSTEM_CONTROL must be an absolute path",
        ),
        (
            vec![
                ("AGORA_SANDBOX_REMOTE_CONTROL", Some("relative")),
                ("AGORA_SANDBOX_REMOTE_TOKEN", Some("token")),
                ("AGORA_SANDBOX_REMOTE_ROOTS", Some("[]")),
            ],
            "AGORA_SANDBOX_REMOTE_CONTROL must be an absolute path",
        ),
        (
            vec![
                ("AGORA_SANDBOX_REMOTE_CONTROL", Some("/tmp/remote.sock")),
                ("AGORA_SANDBOX_REMOTE_TOKEN", Some("token")),
                ("AGORA_SANDBOX_REMOTE_ROOTS", Some("not-json")),
            ],
            "invalid AGORA_SANDBOX_REMOTE_ROOTS",
        ),
        (
            vec![
                ("AGORA_SANDBOX_REMOTE_CONTROL", Some("/tmp/remote.sock")),
                ("AGORA_SANDBOX_REMOTE_TOKEN", Some("token")),
                ("AGORA_SANDBOX_REMOTE_ROOTS", Some("[]")),
            ],
            "AGORA_SANDBOX_REMOTE_ROOTS cannot be empty",
        ),
    ] {
        let error = parse(&overrides).unwrap_err();
        assert!(
            error.contains(expected),
            "{error:?} did not contain {expected:?}"
        );
    }
}

#[test]
fn raw_socket_decoder_rejects_null_short_and_unknown_addresses() {
    assert_eq!(unsafe { socket_addr_from_raw(std::ptr::null(), 0) }, None);

    let mut unknown: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let address = std::ptr::addr_of_mut!(unknown).cast::<libc::sockaddr>();
    unsafe {
        (*address).sa_family = libc::AF_UNIX as libc::sa_family_t;
    }
    assert_eq!(unsafe { socket_addr_from_raw(address, 1) }, None);
    assert_eq!(
        unsafe {
            socket_addr_from_raw(
                address,
                std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            )
        },
        None
    );
}
