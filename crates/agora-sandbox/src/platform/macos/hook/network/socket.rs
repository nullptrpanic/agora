use std::mem::{self, MaybeUninit};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

pub(in crate::platform::hook) struct RawSocketAddress {
    storage: libc::sockaddr_storage,
    length: libc::socklen_t,
}

impl RawSocketAddress {
    pub(in crate::platform::hook) fn new(address: SocketAddr) -> Self {
        let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
        let length = unsafe {
            match address {
                SocketAddr::V4(address) => {
                    let raw = storage.as_mut_ptr().cast::<libc::sockaddr_in>();
                    #[cfg(target_os = "macos")]
                    {
                        (*raw).sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
                    }
                    (*raw).sin_family = libc::AF_INET as libc::sa_family_t;
                    (*raw).sin_port = address.port().to_be();
                    (*raw).sin_addr = libc::in_addr {
                        s_addr: u32::from_ne_bytes(address.ip().octets()),
                    };
                    mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
                }
                SocketAddr::V6(address) => {
                    let raw = storage.as_mut_ptr().cast::<libc::sockaddr_in6>();
                    #[cfg(target_os = "macos")]
                    {
                        (*raw).sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
                    }
                    (*raw).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                    (*raw).sin6_port = address.port().to_be();
                    (*raw).sin6_flowinfo = address.flowinfo();
                    (*raw).sin6_addr = libc::in6_addr {
                        s6_addr: address.ip().octets(),
                    };
                    (*raw).sin6_scope_id = address.scope_id();
                    mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
                }
            }
        };
        Self {
            storage: unsafe { storage.assume_init() },
            length,
        }
    }

    pub(in crate::platform::hook) fn as_ptr(&self) -> *const libc::sockaddr {
        std::ptr::addr_of!(self.storage).cast()
    }

    pub(in crate::platform::hook) fn len(&self) -> libc::socklen_t {
        self.length
    }
}

pub(in crate::platform::hook) unsafe fn socket_addr_from_raw(
    address: *const libc::sockaddr,
    length: libc::socklen_t,
) -> Option<SocketAddr> {
    if address.is_null() || length < mem::size_of::<libc::sockaddr>() as libc::socklen_t {
        return None;
    }
    let family = unsafe { (*address).sa_family as libc::c_int };
    match family {
        libc::AF_INET if length >= mem::size_of::<libc::sockaddr_in>() as libc::socklen_t => {
            let raw = unsafe { &*address.cast::<libc::sockaddr_in>() };
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(raw.sin_addr.s_addr.to_ne_bytes()),
                u16::from_be(raw.sin_port),
            )))
        }
        libc::AF_INET6 if length >= mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t => {
            let raw = unsafe { &*address.cast::<libc::sockaddr_in6>() };
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(raw.sin6_addr.s6_addr),
                u16::from_be(raw.sin6_port),
                raw.sin6_flowinfo,
                raw.sin6_scope_id,
            )))
        }
        _ => None,
    }
}
