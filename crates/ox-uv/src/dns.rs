//! Pool-backed DNS utilities using the safe standard networking API.

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use crate::UvLoop;
use crate::pool::{LoopPoster, Pool, PoolError};

/// Address-family filter for name resolution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AddressFamily {
    /// No address family preference.
    #[default]
    Unspecified,
    /// IPv4 only.
    Inet,
    /// IPv6 only.
    Inet6,
}

/// Socket type represented in a resolved address record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SocketType {
    /// Stream-oriented socket.
    #[default]
    Stream,
    /// Datagram socket.
    Datagram,
}

/// Safe subset of `getaddrinfo` hints supported without libc.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AddrInfoHints {
    /// Address family filter.
    pub family: AddressFamily,
    /// Socket type filter.
    pub socket_type: SocketType,
    /// IP protocol number, if specified.
    pub protocol: Option<u16>,
    /// Use a passive bind address when the host is absent.
    pub passive: bool,
    /// Require the host to be a numeric IP literal.
    pub numeric_host: bool,
}

/// One resolved address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AddrInfo {
    /// Resolved IP address.
    pub address: IpAddr,
    /// Address family of the resolved address.
    pub family: AddressFamily,
    /// Port number.
    pub port: u16,
    /// Socket type.
    pub socket_type: SocketType,
    /// Transport protocol name.
    pub protocol: &'static str,
    /// Canonical name, when available.
    pub canonical_name: Option<String>,
}

/// Host and service names returned for a socket address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameInfo {
    /// Host address or name.
    pub host: String,
    /// Service name or port number.
    pub service: String,
}

/// DNS lookup failure using libuv/getaddrinfo-style names.
#[derive(Debug, thiserror::Error)]
#[error("{name}: {message}")]
pub struct DnsError {
    /// Stable getaddrinfo-style error name.
    pub name: &'static str,
    /// Platform error message.
    pub message: String,
}

impl DnsError {
    fn invalid(message: impl Into<String>) -> Self { Self { name: "EAI_NONAME", message: message.into() } }
    fn io(error: io::Error) -> Self { Self { name: "EAI_FAIL", message: error.to_string() } }
    fn pool(error: PoolError) -> Self { Self { name: "ECANCELED", message: error.to_string() } }
}

/// Result type for DNS operations.
pub type DnsResult<T> = Result<T, DnsError>;

/// Resolves a host and numeric service using safe `ToSocketAddrs`.
///
/// `addrconfig`, `v4mapped`, `all`, named protocols/services, and canonical-name
/// discovery require libc APIs and are intentionally outside this safe subset.
/// See `uv.getaddrinfo()` in `runtime/doc/luvref.txt`.
pub fn getaddrinfo(host: Option<&str>, service: Option<&str>, hints: AddrInfoHints) -> DnsResult<Vec<AddrInfo>> {
    if host.is_none() && service.is_none() { return Err(DnsError::invalid("host and service cannot both be absent")); }
    let port = match service { Some(value) => value.parse::<u16>().map_err(|_| DnsError::invalid("only numeric services are supported"))?, None => 0 };
    let host = match host { Some(value) => value.to_owned(), None if hints.passive => match hints.family { AddressFamily::Inet6 => "::".into(), _ => "0.0.0.0".into() }, None => match hints.family { AddressFamily::Inet6 => "::1".into(), _ => "127.0.0.1".into() } };
    let addresses: Vec<SocketAddr> = if hints.numeric_host {
        vec![SocketAddr::new(host.parse::<IpAddr>().map_err(|_| DnsError::invalid("numeric_host requires an IP literal"))?, port)]
    } else {
        (host.as_str(), port).to_socket_addrs().map_err(DnsError::io)?.collect()
    };
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for address in addresses {
        let family = if address.is_ipv4() { AddressFamily::Inet } else { AddressFamily::Inet6 };
        if hints.family != AddressFamily::Unspecified && hints.family != family { continue; }
        if seen.insert(address) {
            result.push(AddrInfo { address: address.ip(), family, port: address.port(), socket_type: hints.socket_type, protocol: match hints.socket_type { SocketType::Stream => "tcp", SocketType::Datagram => "udp" }, canonical_name: None });
        }
    }
    if result.is_empty() { Err(DnsError::invalid("no addresses matched the requested family")) } else { Ok(result) }
}

/// Returns the portable numeric host and service for an address.
///
/// Safe Rust's standard library does not expose reverse DNS; numeric output is
/// the portable `getnameinfo` form and avoids libc. See `uv.getnameinfo()` in
/// `runtime/doc/luvref.txt`.
pub fn getnameinfo(address: SocketAddr) -> DnsResult<NameInfo> {
    Ok(NameInfo { host: address.ip().to_string(), service: address.port().to_string() })
}

/// Resolves on the shared pool and posts completion to the loop.
/// See `uv.getaddrinfo()` in `runtime/doc/luvref.txt`.
pub fn getaddrinfo_async<P, C>(pool: &Pool, poster: P, host: Option<String>, service: Option<String>, hints: AddrInfoHints, callback: C) -> Result<(), PoolError>
where P: LoopPoster, C: FnOnce(&mut UvLoop, DnsResult<Vec<AddrInfo>>) + Send + 'static {
    pool.submit(poster, move || getaddrinfo(host.as_deref(), service.as_deref(), hints), move |uv_loop, result| callback(uv_loop, result.unwrap_or_else(|error| Err(DnsError::pool(error)))))
}

/// Produces name information on the pool and posts completion to the loop.
/// See `uv.getnameinfo()` in `runtime/doc/luvref.txt`.
pub fn getnameinfo_async<P, C>(pool: &Pool, poster: P, address: SocketAddr, callback: C) -> Result<(), PoolError>
where P: LoopPoster, C: FnOnce(&mut UvLoop, DnsResult<NameInfo>) + Send + 'static {
    pool.submit(poster, move || getnameinfo(address), move |uv_loop, result| callback(uv_loop, result.unwrap_or_else(|error| Err(DnsError::pool(error)))))
}
