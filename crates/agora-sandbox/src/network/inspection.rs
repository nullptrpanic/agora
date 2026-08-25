use crate::callback::DomainSource;
use rustls::server::Acceptor;
use std::io::Cursor;
use std::net::IpAddr;

pub(super) const MAX_INSPECTION_BYTES: usize = 64 * 1024;
const MAX_HTTP_HEADERS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DomainObservation {
    pub(super) domain: String,
    pub(super) source: DomainSource,
    pub(super) target_port: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct InspectionObservation {
    pub(super) domain: Option<DomainObservation>,
    pub(super) tls: Option<TlsClientHello>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TlsClientHello {
    pub(super) server_name: Option<String>,
    pub(super) alpn: Vec<Vec<u8>>,
}

pub(super) struct ProtocolInspector {
    buffer: Vec<u8>,
    protocol: Protocol,
}

impl ProtocolInspector {
    pub(super) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            protocol: Protocol::Unknown,
        }
    }

    pub(super) fn inspect(&mut self, bytes: &[u8]) -> InspectionState {
        if self.protocol == Protocol::Done {
            return InspectionState::Complete(InspectionObservation::default());
        }
        if bytes.is_empty() {
            return InspectionState::Pending;
        }
        if self.buffer.len().saturating_add(bytes.len()) > MAX_INSPECTION_BYTES {
            self.finish();
            return InspectionState::Complete(InspectionObservation::default());
        }
        self.buffer.extend_from_slice(bytes);
        if self.protocol == Protocol::Unknown {
            self.protocol = match self.buffer.first() {
                Some(0x16) => Protocol::Tls,
                Some(first) if first.is_ascii_alphabetic() => Protocol::Http,
                Some(_) => Protocol::Done,
                None => Protocol::Unknown,
            };
        }

        let result = match self.protocol {
            Protocol::Http => Self::inspect_http(&self.buffer),
            Protocol::Tls => Self::inspect_tls(&self.buffer),
            Protocol::Unknown | Protocol::Done => {
                InspectionResult::Complete(InspectionObservation::default())
            }
        };
        match result {
            InspectionResult::Pending => InspectionState::Pending,
            InspectionResult::Complete(observation) => {
                self.finish();
                InspectionState::Complete(observation)
            }
            InspectionResult::Invalid => {
                self.finish();
                InspectionState::Complete(InspectionObservation::default())
            }
        }
    }

    fn inspect_http(bytes: &[u8]) -> InspectionResult {
        let mut headers = [httparse::EMPTY_HEADER; MAX_HTTP_HEADERS];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(bytes) {
            Ok(httparse::Status::Partial) => InspectionResult::Pending,
            Ok(httparse::Status::Complete(_)) => {
                let domain = request
                    .headers
                    .iter()
                    .find(|header| header.name.eq_ignore_ascii_case("host"))
                    .and_then(|header| Self::normalize_domain(header.value));
                let target_port = match (request.method, request.path, domain.as_deref()) {
                    (Some("CONNECT"), Some(target), Some(domain)) => {
                        Self::parse_connect_target(target)
                            .filter(|(target_domain, _)| target_domain == domain)
                            .map(|(_, port)| port)
                    }
                    _ => None,
                };
                let observation = domain.map(|domain| DomainObservation {
                    domain,
                    source: DomainSource::HttpHost,
                    target_port,
                });
                InspectionResult::Complete(InspectionObservation {
                    domain: observation,
                    tls: None,
                })
            }
            Err(_) => InspectionResult::Invalid,
        }
    }

    fn inspect_tls(bytes: &[u8]) -> InspectionResult {
        let mut acceptor = Acceptor::default();
        let mut reader = Cursor::new(bytes);
        if acceptor.read_tls(&mut reader).is_err() {
            return InspectionResult::Invalid;
        }
        match acceptor.accept() {
            Ok(None) => InspectionResult::Pending,
            Ok(Some(accepted)) => {
                let hello = accepted.client_hello();
                let server_name = hello
                    .server_name()
                    .and_then(|value| Self::normalize_domain(value.as_bytes()));
                let domain = server_name.as_ref().map(|value| DomainObservation {
                    domain: value.clone(),
                    source: DomainSource::TlsSni,
                    target_port: None,
                });
                let alpn = hello
                    .alpn()
                    .map(|protocols| protocols.map(<[u8]>::to_vec).collect())
                    .unwrap_or_default();
                InspectionResult::Complete(InspectionObservation {
                    domain,
                    tls: Some(TlsClientHello { server_name, alpn }),
                })
            }
            Err(_) => InspectionResult::Invalid,
        }
    }

    fn normalize_domain(value: &[u8]) -> Option<String> {
        let value = std::str::from_utf8(value).ok()?.trim();
        let host = if let Some(bracketed) = value.strip_prefix('[') {
            bracketed.split_once(']')?.0
        } else if let Some((host, port)) = value.rsplit_once(':') {
            if port.parse::<u16>().is_ok() {
                host
            } else {
                value
            }
        } else {
            value
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() || host.parse::<IpAddr>().is_ok() {
            None
        } else {
            Some(host)
        }
    }

    fn parse_connect_target(value: &str) -> Option<(String, u16)> {
        let (host, port) = value.rsplit_once(':')?;
        let port = port.parse::<u16>().ok()?;
        let domain = Self::normalize_domain(host.as_bytes())?;
        Some((domain, port))
    }

    fn finish(&mut self) {
        self.protocol = Protocol::Done;
        self.buffer.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InspectionState {
    Pending,
    Complete(InspectionObservation),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Unknown,
    Http,
    Tls,
    Done,
}

enum InspectionResult {
    Pending,
    Complete(InspectionObservation),
    Invalid,
}
