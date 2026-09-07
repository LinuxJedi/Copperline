// SPDX-License-Identifier: GPL-3.0-or-later

//! Nonblocking packet adapters. Reliability belongs to the shared protocol.

use super::wire::MAX_PACKET;
use anyhow::{ensure, Result};
use std::collections::VecDeque;

pub trait Transport {
    fn route(&self) -> &'static str {
        "direct"
    }
    /// Connection setup may continue off-thread while the cold machine waits.
    fn ready(&mut self) -> Result<bool> {
        Ok(true)
    }
    /// Read one complete packet without blocking. None means the queue is empty;
    /// a returned length must fit the supplied buffer. Some(0) means a packet
    /// was consumed and discarded (for example, a foreign UDP source).
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>>;
    /// False means the transport is temporarily unable to accept this packet.
    fn send(&mut self, packet: &[u8]) -> Result<bool>;
}

#[cfg(not(target_arch = "wasm32"))]
pub enum NativeTransport {
    Udp(UdpTransport),
    #[cfg(feature = "netplay-internet")]
    Internet(Box<super::internet::InternetTransport>),
}

#[cfg(not(target_arch = "wasm32"))]
impl Transport for NativeTransport {
    fn route(&self) -> &'static str {
        match self {
            Self::Udp(_) => "UDP",
            #[cfg(feature = "netplay-internet")]
            Self::Internet(t) => t.route(),
        }
    }
    fn ready(&mut self) -> Result<bool> {
        match self {
            Self::Udp(t) => t.ready(),
            #[cfg(feature = "netplay-internet")]
            Self::Internet(t) => t.ready(),
        }
    }
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        match self {
            Self::Udp(t) => t.receive(buffer),
            #[cfg(feature = "netplay-internet")]
            Self::Internet(t) => t.receive(buffer),
        }
    }
    fn send(&mut self, packet: &[u8]) -> Result<bool> {
        match self {
            Self::Udp(t) => t.send(packet),
            #[cfg(feature = "netplay-internet")]
            Self::Internet(t) => t.send(packet),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
impl NativeTransport {
    pub(super) fn socket(&self) -> &std::net::UdpSocket {
        match self {
            Self::Udp(t) => &t.socket,
            #[cfg(feature = "netplay-internet")]
            _ => panic!("expected UDP transport"),
        }
    }
}

/// The browser feeds incoming data-channel packets and drains outgoing packets.
/// Both queues are bounded independently of how often JavaScript services them.
#[derive(Default)]
pub struct PacketQueue {
    incoming: VecDeque<Vec<u8>>,
    outgoing: VecDeque<Vec<u8>>,
}

impl PacketQueue {
    pub fn push(&mut self, packet: &[u8]) -> Result<()> {
        ensure!(packet.len() <= MAX_PACKET, "netplay packet is too large");
        if self.incoming.len() == 64 {
            self.incoming.pop_front();
        }
        self.incoming.push_back(packet.to_vec());
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }
}

impl Transport for PacketQueue {
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        let Some(packet) = self.incoming.pop_front() else {
            return Ok(None);
        };
        ensure!(
            packet.len() <= buffer.len(),
            "netplay receive buffer is too small"
        );
        buffer[..packet.len()].copy_from_slice(&packet);
        Ok(Some(packet.len()))
    }

    fn send(&mut self, packet: &[u8]) -> Result<bool> {
        ensure!(packet.len() <= MAX_PACKET, "netplay packet is too large");
        if self.outgoing.len() == 64 {
            return Ok(false);
        }
        self.outgoing.push_back(packet.to_vec());
        Ok(true)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub struct UdpTransport {
    pub(super) socket: std::net::UdpSocket,
    pub(super) options: super::Options,
}

#[cfg(not(target_arch = "wasm32"))]
impl UdpTransport {
    pub(super) fn new(options: super::Options) -> Result<Self> {
        use anyhow::Context;
        let socket =
            std::net::UdpSocket::bind(options.bind).context("binding netplay UDP socket")?;
        socket.set_nonblocking(true)?;
        log::info!(
            "netplay: listening on {}, peer {}, player {}; waiting for matching machine",
            socket.local_addr()?,
            options.peer,
            options.player + 1
        );
        Ok(Self { socket, options })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Transport for UdpTransport {
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        match self.socket.recv_from(buffer) {
            Ok((len, source)) => Ok(Some(if source == self.options.peer { len } else { 0 })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn send(&mut self, packet: &[u8]) -> Result<bool> {
        match self.socket.send_to(packet, self.options.peer) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
