//! The server side of SOCKS5 (RFC 1928) as far as a tunnel needs it: no authentication, the
//! CONNECT command, IPv4 / IPv6 / domain targets. Domains are not resolved here — they travel
//! to the SSH server as the target of the `direct-tcpip` channel and are resolved there.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};

use shellmate_ssh::{SshError, TcpipFailure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VERSION: u8 = 5;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NONE_ACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// Reply codes (the `REP` field).
pub(crate) mod reply {
    pub const SUCCEEDED: u8 = 0x00;
    pub const GENERAL_FAILURE: u8 = 0x01;
    pub const NOT_ALLOWED: u8 = 0x02;
    pub const HOST_UNREACHABLE: u8 = 0x04;
    pub const CONNECTION_REFUSED: u8 = 0x05;
    pub const COMMAND_NOT_SUPPORTED: u8 = 0x07;
    pub const ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;
}

/// Where a SOCKS client wants to be connected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// An IP address in text form, or a domain name.
    pub host: String,
    pub port: u16,
}

/// How the exchange before the data phase ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Negotiated {
    /// The client asked to CONNECT.
    Connect(Request),
    /// Nothing to forward: the request was refused (and answered if the protocol allows it)
    /// or was not SOCKS5 at all.
    Refused,
}

/// Reads the greeting and the request; answers the greeting, and the request too if it cannot
/// be served. On [`Negotiated::Connect`] the caller still owes the reply to the request
/// ([`send_reply`]) once it knows whether the target can be reached.
///
/// Reads exactly the bytes of the two messages, so whatever the client already sent after them
/// stays in the socket for the data phase.
pub(crate) async fn negotiate<S>(stream: &mut S) -> io::Result<Negotiated>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Greeting: VER, NMETHODS, METHODS...
    let version = stream.read_u8().await?;
    if version != VERSION {
        // SOCKS4 (or something else entirely): not served, closed without a word.
        return Ok(Negotiated::Refused);
    }
    let count = stream.read_u8().await?;
    let mut methods = vec![0u8; count as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&METHOD_NO_AUTH) {
        stream.write_all(&[VERSION, METHOD_NONE_ACCEPTABLE]).await?;
        return Ok(Negotiated::Refused);
    }
    stream.write_all(&[VERSION, METHOD_NO_AUTH]).await?;

    // Request: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let [version, command, _reserved, address_type] = head;
    if version != VERSION {
        return Ok(Negotiated::Refused);
    }
    let host = match address_type {
        ATYP_IPV4 => {
            let mut octets = [0u8; 4];
            stream.read_exact(&mut octets).await?;
            Ipv4Addr::from(octets).to_string()
        }
        ATYP_IPV6 => {
            let mut octets = [0u8; 16];
            stream.read_exact(&mut octets).await?;
            Ipv6Addr::from(octets).to_string()
        }
        ATYP_DOMAIN => {
            let length = stream.read_u8().await?;
            let mut name = vec![0u8; length as usize];
            stream.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(name) if !name.is_empty() => name,
                _ => {
                    // The port still follows; the client is not going to read it.
                    send_reply(stream, reply::GENERAL_FAILURE).await?;
                    return Ok(Negotiated::Refused);
                }
            }
        }
        _ => {
            send_reply(stream, reply::ADDRESS_TYPE_NOT_SUPPORTED).await?;
            return Ok(Negotiated::Refused);
        }
    };
    let port = stream.read_u16().await?;
    if command != CMD_CONNECT {
        // BIND and UDP ASSOCIATE.
        send_reply(stream, reply::COMMAND_NOT_SUPPORTED).await?;
        return Ok(Negotiated::Refused);
    }
    if port == 0 {
        send_reply(stream, reply::GENERAL_FAILURE).await?;
        return Ok(Negotiated::Refused);
    }
    Ok(Negotiated::Connect(Request { host, port }))
}

/// Sends a reply to the request. The bound address is always `0.0.0.0:0`: the connection is
/// made by the server, there is no local address worth reporting.
pub(crate) async fn send_reply<S>(stream: &mut S, code: u8) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream.write_all(&[VERSION, code, 0, ATYP_IPV4, 0, 0, 0, 0, 0, 0]).await?;
    stream.flush().await
}

/// The reply code for a `direct-tcpip` channel that could not be opened.
pub(crate) fn code_for(error: &SshError) -> u8 {
    match error {
        SshError::Tcpip { reason, .. } => match reason {
            TcpipFailure::Prohibited => reply::NOT_ALLOWED,
            TcpipFailure::ConnectFailed => reply::CONNECTION_REFUSED,
            TcpipFailure::Timeout => reply::HOST_UNREACHABLE,
            TcpipFailure::ResourceShortage | TcpipFailure::Disconnected | TcpipFailure::Other => reply::GENERAL_FAILURE,
        },
        _ => reply::GENERAL_FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `negotiate` against `client_bytes` and returns the outcome and what the server said.
    async fn run(client_bytes: &[u8]) -> (io::Result<Negotiated>, Vec<u8>) {
        let (mut client, mut server) = tokio::io::duplex(4096);
        client.write_all(client_bytes).await.unwrap();
        client.shutdown().await.unwrap();
        let outcome = negotiate(&mut server).await;
        drop(server);
        let mut said = Vec::new();
        client.read_to_end(&mut said).await.unwrap();
        (outcome, said)
    }

    fn connect_request(address: &[u8], port: u16) -> Vec<u8> {
        // Greeting (one method: no authentication), then the request header; `address` starts
        // with the address type byte.
        let mut bytes = vec![5, 1, 0, 5, 1, 0];
        bytes.extend_from_slice(address);
        bytes.extend_from_slice(&port.to_be_bytes());
        bytes
    }

    const GREETED: [u8; 2] = [5, 0];

    #[tokio::test]
    async fn connect_to_an_ipv4_address() {
        let (outcome, said) = run(&connect_request(&[1, 10, 0, 0, 5], 5432)).await;
        assert_eq!(outcome.unwrap(), Negotiated::Connect(Request { host: "10.0.0.5".into(), port: 5432 }));
        assert_eq!(said, GREETED, "only the greeting is answered; the reply waits for the channel");
    }

    #[tokio::test]
    async fn connect_to_an_ipv6_address() {
        let mut address = vec![4];
        address.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        let (outcome, _) = run(&connect_request(&address, 443)).await;
        assert_eq!(outcome.unwrap(), Negotiated::Connect(Request { host: "::1".into(), port: 443 }));
    }

    #[tokio::test]
    async fn a_domain_is_passed_on_unresolved() {
        let mut address = vec![3, 9];
        address.extend_from_slice(b"localhost");
        let (outcome, _) = run(&connect_request(&address, 22)).await;
        assert_eq!(outcome.unwrap(), Negotiated::Connect(Request { host: "localhost".into(), port: 22 }));
    }

    #[tokio::test]
    async fn bind_and_udp_are_not_supported() {
        for command in [2u8, 3] {
            let mut bytes = vec![5, 1, 0, 5, command, 0, 1, 127, 0, 0, 1];
            bytes.extend_from_slice(&80u16.to_be_bytes());
            let (outcome, said) = run(&bytes).await;
            assert_eq!(outcome.unwrap(), Negotiated::Refused);
            assert_eq!(said, [5, 0, 5, 0x07, 0, 1, 0, 0, 0, 0, 0, 0], "command {command}");
        }
    }

    #[tokio::test]
    async fn unknown_address_types_are_refused() {
        let (outcome, said) = run(&[5, 1, 0, 5, 1, 0, 9, 1, 2, 3]).await;
        assert_eq!(outcome.unwrap(), Negotiated::Refused);
        assert_eq!(&said[2..4], &[5, 0x08]);
    }

    #[tokio::test]
    async fn socks4_is_closed_without_an_answer() {
        // SOCKS4 CONNECT to 1.2.3.4:80.
        let (outcome, said) = run(&[4, 1, 0, 80, 1, 2, 3, 4, 0]).await;
        assert_eq!(outcome.unwrap(), Negotiated::Refused);
        assert!(said.is_empty());
        let (outcome, said) = run(b"GET / HTTP/1.1\r\n\r\n").await;
        assert_eq!(outcome.unwrap(), Negotiated::Refused);
        assert!(said.is_empty());
    }

    #[tokio::test]
    async fn clients_that_need_authentication_are_turned_away() {
        let (outcome, said) = run(&[5, 1, 2]).await;
        assert_eq!(outcome.unwrap(), Negotiated::Refused);
        assert_eq!(said, [5, 0xFF]);
        let (_, said) = run(&[5, 0]).await;
        assert_eq!(said, [5, 0xFF], "no methods at all");
    }

    #[tokio::test]
    async fn port_zero_and_empty_domains_fail_generally() {
        let (outcome, said) = run(&connect_request(&[1, 1, 2, 3, 4], 0)).await;
        assert_eq!(outcome.unwrap(), Negotiated::Refused);
        assert_eq!(said[3], 0x01);
        let (outcome, said) = run(&connect_request(&[3, 0], 80)).await;
        assert_eq!(outcome.unwrap(), Negotiated::Refused);
        assert_eq!(said[3], 0x01);
    }

    #[tokio::test]
    async fn a_truncated_request_is_an_io_error() {
        let (outcome, _) = run(&[5, 1, 0, 5, 1, 0, 1, 10, 0]).await;
        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn bytes_after_the_request_are_left_for_the_data_phase() {
        let mut bytes = connect_request(&[1, 127, 0, 0, 1], 80);
        bytes.extend_from_slice(b"early data");
        let (mut client, mut server) = tokio::io::duplex(4096);
        client.write_all(&bytes).await.unwrap();
        assert!(matches!(negotiate(&mut server).await.unwrap(), Negotiated::Connect(_)));
        let mut rest = [0u8; 10];
        server.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"early data");
    }

    #[test]
    fn open_failures_map_to_reply_codes() {
        let failure = |reason| SshError::Tcpip { host: "h".into(), port: 1, reason };
        assert_eq!(code_for(&failure(TcpipFailure::ConnectFailed)), 0x05);
        assert_eq!(code_for(&failure(TcpipFailure::Prohibited)), 0x02);
        assert_eq!(code_for(&failure(TcpipFailure::Timeout)), 0x04);
        assert_eq!(code_for(&failure(TcpipFailure::ResourceShortage)), 0x01);
        assert_eq!(code_for(&failure(TcpipFailure::Disconnected)), 0x01);
        assert_eq!(code_for(&failure(TcpipFailure::Other)), 0x01);
        assert_eq!(code_for(&SshError::RemoteForwardDenied), 0x01);
    }
}
