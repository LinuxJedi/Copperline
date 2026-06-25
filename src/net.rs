// SPDX-License-Identifier: GPL-3.0-or-later

//! Host networking backends for emulated Ethernet boards (the a2065 LANCE and
//! WASM NIC plugins).
//!
//! An emulated NIC owns a [`NetBackend`]: it pushes the Ethernet frames the
//! guest transmits with [`NetBackend::send`] and pulls inbound frames with
//! [`NetBackend::poll`]. A frame is a complete Ethernet frame (destination MAC
//! through payload, no FCS).
//!
//! Networking is inherently non-deterministic: inbound frames arrive on the
//! host's schedule, not the emulated clock, so a NIC board breaks Copperline's
//! byte-identical replay / save-state determinism while traffic flows. Backends
//! are therefore host resources, not serialized state -- a save state records
//! only the board's chosen backend ([`NetConfig`]) and brings up a fresh
//! backend on load (in-flight frames are dropped; the guest's TCP retransmits).
//!
//! One backend is built in: [`LoopbackBackend`] (frames echo back to the
//! sender, for tests and a self-contained two-station demo). [`NetConfig::None`]
//! brings up no backend at all, which is how an isolated NIC (one with the
//! capability but no host connectivity) is expressed. Userspace NAT
//! (libslirp/smoltcp) and a host TAP bridge are planned and will slot in behind
//! [`make_backend`] under build features.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A host networking backend an emulated NIC sends and receives frames through.
/// `Send` so it can live in a wasmtime store's data (which the bus owns).
pub trait NetBackend: Send {
    /// Transmit one Ethernet frame from the guest to the network.
    fn send(&mut self, frame: &[u8]);

    /// Return the next inbound Ethernet frame for the guest, if any.
    fn poll(&mut self) -> Option<Vec<u8>>;
}

/// Which host backend a NIC board uses. Recorded in the board's config (and
/// save state) so the board is self-contained; the live backend it names is a
/// host resource brought up fresh by [`make_backend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NetConfig {
    /// No connectivity: transmits are dropped, nothing is ever received.
    #[default]
    None,
    /// Frames transmitted are queued straight back as received frames. Lets a
    /// guest see its own broadcasts and supports a self-contained demo without
    /// touching the host network; also the deterministic backend for tests.
    Loopback,
}

/// Bring up the live backend a [`NetConfig`] names. `None` means the board has
/// no host networking (its NIC still works, it just never sees traffic).
pub fn make_backend(cfg: NetConfig) -> Option<Box<dyn NetBackend>> {
    match cfg {
        NetConfig::None => None,
        NetConfig::Loopback => Some(Box::new(LoopbackBackend::default())),
    }
}

/// A backend that queues each transmitted frame straight back for receipt.
#[derive(Default)]
pub struct LoopbackBackend {
    queue: VecDeque<Vec<u8>>,
}

impl NetBackend for LoopbackBackend {
    fn send(&mut self, frame: &[u8]) {
        self.queue.push_back(frame.to_vec());
    }

    fn poll(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front()
    }
}

/// Parse a `net`/`net_backend` config string into a [`NetConfig`].
pub fn parse_net_config(s: &str) -> Option<NetConfig> {
    match s.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "" => Some(NetConfig::None),
        "loopback" | "loop" => Some(NetConfig::Loopback),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_returns_sent_frames_in_order() {
        let mut b = LoopbackBackend::default();
        assert!(b.poll().is_none());
        b.send(&[1, 2, 3]);
        b.send(&[4, 5]);
        assert_eq!(b.poll(), Some(vec![1, 2, 3]));
        assert_eq!(b.poll(), Some(vec![4, 5]));
        assert!(b.poll().is_none());
    }

    #[test]
    fn config_parses_known_backends() {
        assert_eq!(parse_net_config("loopback"), Some(NetConfig::Loopback));
        assert_eq!(parse_net_config("None"), Some(NetConfig::None));
        assert_eq!(parse_net_config(""), Some(NetConfig::None));
        assert_eq!(parse_net_config("tap0"), None);
    }

    #[test]
    fn make_backend_brings_up_named_backend() {
        assert!(make_backend(NetConfig::None).is_none());
        let mut b = make_backend(NetConfig::Loopback).expect("loopback backend");
        b.send(&[9]);
        assert_eq!(b.poll(), Some(vec![9]));
    }
}
