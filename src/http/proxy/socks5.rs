//! SOCKS5 CONNECT negotiation (RFC 1928) and username/password auth (RFC 1929).
//!
//! The caller owns the socket and the connect/header deadline. This module
//! never starts a detached task, sends an HTTP request, or falls back to a
//! direct connection. Exact-sized reads preserve the first origin bytes.

use asupersync::io::ext::{AsyncReadExt, AsyncWriteExt};
use asupersync::io::{AsyncRead, AsyncWrite};
use std::fmt;
use std::io;
use std::net::IpAddr;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Socks5Config {
    pub remote_dns: bool,
    credentials: Option<(Vec<u8>, Vec<u8>)>,
}

impl fmt::Debug for Socks5Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Socks5Config")
            .field("remote_dns", &self.remote_dns)
            .field("credentials", &self.credentials.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Socks5Config {
    pub(super) fn new(
        remote_dns: bool,
        credentials: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Result<Self, String> {
        if credentials.as_ref().is_some_and(|(username, password)| {
            !(1..=255).contains(&username.len()) || !(1..=255).contains(&password.len())
        }) {
            return Err("SOCKS5 credentials must each contain 1 to 255 bytes".into());
        }
        Ok(Self { remote_dns, credentials })
    }

    pub(super) const fn scheme(&self) -> &'static str {
        if self.remote_dns { "socks5h" } else { "socks5" }
    }
}

fn protocol_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Build a request before sending negotiation bytes. socks5h never calls a
/// local resolver for the destination; IP literals use their binary form in
/// either mode. The proxy endpoint itself is resolved by the caller.
pub(super) async fn connect_request(
    host: &str,
    port: u16,
    remote_dns: bool,
) -> io::Result<Vec<u8>> {
    if port == 0 || host.is_empty() || host.len() > 255 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid SOCKS5 destination"));
    }
    if let Some(inner) = host.strip_prefix('[') {
        let address = inner
            .strip_suffix(']')
            .and_then(|host| host.parse::<std::net::Ipv6Addr>().ok())
            .ok_or_else(|| io::Error::new(
                io::ErrorKind::InvalidInput, "invalid SOCKS5 destination IPv6 address",
            ))?;
        return Ok(ip_request(address.into(), port));
    }
    if host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid SOCKS5 destination"));
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(ip_request(address, port));
    }
    if remote_dns {
        return domain_request(host, port);
    }
    let lookup = asupersync::net::dns::Resolver::new()
        .lookup_ip(host)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "SOCKS5 local DNS lookup failed"))?;
    let address = lookup.addresses().first().copied().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "SOCKS5 local DNS returned no addresses")
    })?;
    Ok(ip_request(address, port))
}

fn ip_request(address: IpAddr, port: u16) -> Vec<u8> {
    let mut request = vec![5, 1, 0];
    match address {
        IpAddr::V4(address) => {
            request.push(1);
            request.extend_from_slice(&address.octets());
        }
        IpAddr::V6(address) => {
            request.push(4);
            request.extend_from_slice(&address.octets());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    request
}

fn domain_request(host: &str, port: u16) -> io::Result<Vec<u8>> {
    // The wire carries an octet length, not a Unicode character count. IDNs
    // must arrive in their ASCII (punycode) form rather than being guessed.
    let length = u8::try_from(host.len())
        .ok()
        .filter(|length| *length != 0)
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::InvalidInput, "SOCKS5 destination hostname must contain 1 to 255 bytes",
        ))?;
    if !host.is_ascii()
        || host.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        || host.contains(['[', ']', ':', '/', '\\', '@'])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SOCKS5 remote DNS requires an ASCII hostname (use punycode for IDNs)",
        ));
    }
    let mut request = Vec::with_capacity(host.len() + 7);
    request.extend_from_slice(&[5, 1, 0, 3, length]);
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    Ok(request)
}

/// Negotiate on an already-connected socket. When credentials are configured
/// only username/password is offered: a proxy cannot silently select an
/// unauthenticated downgrade. Authentication failure sends no CONNECT request.
pub(super) async fn handshake<T>(
    stream: &mut T,
    config: &Socks5Config,
    request: &[u8],
) -> io::Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let method = if config.credentials.is_some() { 2 } else { 0 };
    stream.write_all(&[5, 1, method]).await?;
    stream.flush().await?;
    let mut selected = [0; 2];
    stream.read_exact(&mut selected).await?;
    if selected[0] != 5 {
        return Err(protocol_error("SOCKS5 proxy returned an invalid negotiation version"));
    }
    if selected[1] != method {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SOCKS5 proxy did not accept the offered authentication method",
        ));
    }
    if let Some((username, password)) = &config.credentials {
        let mut authentication = Vec::with_capacity(username.len() + password.len() + 3);
        authentication.push(1);
        authentication.push(u8::try_from(username.len()).expect("validated SOCKS5 username"));
        authentication.extend_from_slice(username);
        authentication.push(u8::try_from(password.len()).expect("validated SOCKS5 password"));
        authentication.extend_from_slice(password);
        stream.write_all(&authentication).await?;
        stream.flush().await?;
        let mut reply = [0; 2];
        stream.read_exact(&mut reply).await?;
        if reply[0] != 1 {
            return Err(protocol_error("SOCKS5 proxy returned an invalid authentication version"));
        }
        if reply[1] != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "SOCKS5 proxy authentication failed"));
        }
    }
    stream.write_all(request).await?;
    stream.flush().await?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 5 || reply[2] != 0 {
        return Err(protocol_error("SOCKS5 proxy returned an invalid CONNECT reply"));
    }
    if reply[1] != 0 {
        let (kind, message) = match reply[1] {
            2 => (io::ErrorKind::PermissionDenied, "SOCKS5 proxy denied the destination"),
            3 => (io::ErrorKind::NetworkUnreachable, "SOCKS5 destination network unreachable"),
            4 => (io::ErrorKind::HostUnreachable, "SOCKS5 destination host unreachable"),
            5 => (io::ErrorKind::ConnectionRefused, "SOCKS5 destination refused the connection"),
            6 => (io::ErrorKind::TimedOut, "SOCKS5 destination TTL expired"),
            7 => (io::ErrorKind::Unsupported, "SOCKS5 proxy does not support CONNECT"),
            8 => (io::ErrorKind::Unsupported, "SOCKS5 proxy does not support the address type"),
            _ => (io::ErrorKind::Other, "SOCKS5 proxy failed to connect to the destination"),
        };
        return Err(io::Error::new(kind, message));
    }
    let address_bytes = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0; 1];
            stream.read_exact(&mut length).await?;
            if length[0] == 0 {
                return Err(protocol_error("SOCKS5 proxy returned an empty bound hostname"));
            }
            usize::from(length[0])
        }
        _ => return Err(protocol_error("SOCKS5 proxy returned an unknown bound address type")),
    };
    // At most 255 address octets plus the two-byte port. Do not read even
    // one byte beyond the reply: it belongs to the origin protocol.
    let mut bound = [0; 257];
    stream.read_exact(&mut bound[..address_bytes + 2]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::ReadBuf;
    use std::io::Read;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct WireFixture {
        incoming: io::Cursor<Vec<u8>>,
        outgoing: Vec<u8>,
        fragment: usize,
    }

    impl WireFixture {
        fn new(bytes: &[u8]) -> Self {
            Self {
                incoming: io::Cursor::new(bytes.to_vec()),
                outgoing: Vec::new(),
                fragment: 1,
            }
        }
    }

    impl AsyncRead for WireFixture {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let mut scratch = [0; 257];
            let capacity = buf.remaining().min(self.fragment).min(scratch.len());
            let count = Read::read(&mut self.incoming, &mut scratch[..capacity])?;
            buf.put_slice(&scratch[..count]);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for WireFixture {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let count = bytes.len().min(self.fragment);
            self.outgoing.extend_from_slice(&bytes[..count]);
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn run_handshake(wire: &mut WireFixture, config: &Socks5Config) -> io::Result<()> {
        futures::executor::block_on(handshake(
            wire,
            config,
            &domain_request("unresolvable.invalid", 443).unwrap(),
        ))
    }

    fn anonymous() -> Socks5Config {
        Socks5Config::new(true, None).unwrap()
    }

    fn authenticated() -> Socks5Config {
        Socks5Config::new(true, Some((b"u:x".to_vec(), vec![b'p', 0xff, 0]))).unwrap()
    }

    #[test]
    fn remote_hostname_request_preserves_name_and_network_order_port() {
        let request = domain_request("unresolvable.invalid", 8443).unwrap();
        let mut expected = vec![5, 1, 0, 3, 20];
        expected.extend_from_slice(b"unresolvable.invalid");
        expected.extend_from_slice(&[0x20, 0xfb]);
        assert_eq!(request, expected);
        let actual = futures::executor::block_on(connect_request("unresolvable.invalid", 8443, true)).unwrap();
        assert_eq!(actual, expected, "remote resolution never needs a local DNS query");
    }

    #[test]
    fn literal_addresses_use_binary_encoding_in_both_dns_modes() {
        for remote in [false, true] {
            let v4 = futures::executor::block_on(connect_request("127.0.0.1", 80, remote)).unwrap();
            assert_eq!(v4, [5, 1, 0, 1, 127, 0, 0, 1, 0, 80]);
            let v6 = futures::executor::block_on(connect_request("[::1]", 443, remote)).unwrap();
            assert_eq!(&v6[..4], &[5, 1, 0, 4]);
            assert_eq!(&v6[4..20], &std::net::Ipv6Addr::LOCALHOST.octets());
            assert_eq!(&v6[20..], &[1, 187]);
        }
    }

    #[test]
    fn remote_names_and_ports_are_bounded_before_negotiation() {
        assert!(domain_request(&"a".repeat(255), 443).is_ok());
        for name in ["".to_string(), "a".repeat(256), "é.example".to_string(), "a/b".to_string(), "a\0b".to_string()] {
            assert!(domain_request(&name, 443).is_err());
        }
        assert!(futures::executor::block_on(connect_request("host", 0, true)).is_err());
    }

    #[test]
    fn anonymous_fragmented_exchange_preserves_origin_bytes() {
        let bytes = [5, 0, 5, 0, 0, 1, 127, 0, 0, 1, 1, 187, b'O', b'K'];
        let mut wire = WireFixture::new(&bytes);
        run_handshake(&mut wire, &anonymous()).unwrap();
        let mut expected = vec![5, 1, 0];
        expected.extend(domain_request("unresolvable.invalid", 443).unwrap());
        assert_eq!(wire.outgoing, expected);
        assert_eq!(wire.incoming.position(), 12);
    }

    #[test]
    fn authenticated_fragmented_exchange_preserves_raw_credential_octets() {
        let mut wire = WireFixture::new(&[5, 2, 1, 0, 5, 0, 0, 1, 0, 0, 0, 0, 0, 1]);
        run_handshake(&mut wire, &authenticated()).unwrap();
        let mut expected = vec![5, 1, 2, 1, 3, b'u', b':', b'x', 3, b'p', 0xff, 0];
        expected.extend(domain_request("unresolvable.invalid", 443).unwrap());
        assert_eq!(wire.outgoing, expected);
    }

    #[test]
    fn configured_credentials_cannot_be_downgraded_to_anonymous() {
        let mut wire = WireFixture::new(&[5, 0]);
        assert_eq!(run_handshake(&mut wire, &authenticated()).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(wire.outgoing, [5, 1, 2], "neither credentials nor CONNECT was sent");
    }

    #[test]
    fn unsupported_methods_and_failed_auth_never_send_connect() {
        for method in [1, 2, 0x80, 0xff] {
            let mut wire = WireFixture::new(&[5, method]);
            assert!(run_handshake(&mut wire, &anonymous()).is_err());
            assert_eq!(wire.outgoing, [5, 1, 0]);
        }
        let mut wire = WireFixture::new(&[5, 2, 1, 1]);
        let error = run_handshake(&mut wire, &authenticated()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(wire.outgoing.len(), 12);
        assert!(!error.to_string().contains("u:x"));
    }

    #[test]
    fn every_truncated_success_reply_is_an_error() {
        let bytes = [5, 0, 5, 0, 0, 1, 127, 0, 0, 1, 1, 187];
        for length in 0..bytes.len() {
            let mut wire = WireFixture::new(&bytes[..length]);
            assert_eq!(run_handshake(&mut wire, &anonymous()).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn invalid_versions_reserved_bytes_and_address_types_are_rejected() {
        for bytes in [
            vec![4, 0],
            vec![5, 0, 4, 0, 0, 1],
            vec![5, 0, 5, 0, 1, 1],
            vec![5, 0, 5, 0, 0, 2],
            vec![5, 0, 5, 0, 0, 3, 0],
        ] {
            let mut wire = WireFixture::new(&bytes);
            assert_eq!(run_handshake(&mut wire, &anonymous()).unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
        let mut wire = WireFixture::new(&[5, 2, 2, 0]);
        assert_eq!(run_handshake(&mut wire, &authenticated()).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn all_bound_address_forms_are_consumed_without_overreading() {
        let replies = [
            vec![1, 127, 0, 0, 1, 0, 1],
            [vec![4], vec![0; 16], vec![0, 1]].concat(),
            [vec![3, 255], vec![b'a'; 255], vec![0, 1]].concat(),
        ];
        for reply in replies {
            let bytes = [vec![5, 0, 5, 0, 0], reply, b"origin".to_vec()].concat();
            let mut wire = WireFixture::new(&bytes);
            wire.fragment = 257;
            run_handshake(&mut wire, &anonymous()).unwrap();
            assert_eq!(wire.incoming.position(), u64::try_from(bytes.len() - 6).unwrap());
        }
    }

    #[test]
    fn connect_failure_codes_are_not_successful_tunnels() {
        for reply in 1..=255 {
            let mut wire = WireFixture::new(&[5, 0, 5, reply, 0, 1]);
            let error = run_handshake(&mut wire, &anonymous()).unwrap_err();
            if reply == 5 { assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused); }
            if reply == 2 { assert_eq!(error.kind(), io::ErrorKind::PermissionDenied); }
            assert_eq!(wire.incoming.position(), 6);
        }
    }

    #[test]
    fn credential_lengths_and_debug_output_are_safe() {
        for (username, password) in [(vec![], vec![1]), (vec![1], vec![]), (vec![1; 256], vec![2]), (vec![1], vec![2; 256])] {
            assert!(Socks5Config::new(true, Some((username, password))).is_err());
        }
        assert!(Socks5Config::new(false, Some((vec![1; 255], vec![2; 255]))).is_ok());
        let config = Socks5Config::new(true, Some((b"sentinel-user".to_vec(), b"sentinel-secret".to_vec()))).unwrap();
        let debug = format!("{config:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("sentinel"));
    }
}
