use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket};

pub fn bind() -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_send_buffer_size(1 << 20)?;
    s.set_nonblocking(false)?;
    // DSCP EF (46<<2) for voice
    s.set_tos(0xB8).ok();
    s.bind(&"0.0.0.0:0".parse::<SocketAddr>().unwrap().into())?;
    Ok(s.into())
}

/// IP discovery: 74-byte packet {type=1,len=70,ssrc,...}; reply carries our public ip:port.
pub fn discover(sock: &UdpSocket, to: SocketAddr, ssrc: u32) -> std::io::Result<(String, u16)> {
    let mut p = [0u8; 74];
    p[0..2].copy_from_slice(&1u16.to_be_bytes());
    p[2..4].copy_from_slice(&70u16.to_be_bytes());
    p[4..8].copy_from_slice(&ssrc.to_be_bytes());
    sock.send_to(&p, to)?;
    let mut r = [0u8; 74];
    sock.recv_from(&mut r)?;
    let ip_end = r[8..72].iter().position(|&b| b == 0).unwrap_or(64);
    let ip = String::from_utf8_lossy(&r[8..8 + ip_end]).to_string();
    let port = u16::from_be_bytes([r[72], r[73]]);
    Ok((ip, port))
}
// Batching: on Linux collect each tick's datagrams from a shard and flush with sendmmsg(2):
// 1000 players -> 1 syscall per tick instead of 1000.
