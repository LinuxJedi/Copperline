// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded reliable messages beside the input datagrams, over either native
//! transport. Selective repeat uses the same socket and peer as netplay.

use super::{Transport, MAX_PACKET};
use crate::timebase::{Duration, Instant};
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, VecDeque};

const MAGIC: &[u8; 4] = b"CLNC";
const HEADER: usize = 4 + 1 + 16 + 1 + 8 + 8 + 4 + 1;
const CHUNK: usize = MAX_PACKET - HEADER;
const WINDOW: u64 = 32;
const RESEND: Duration = Duration::from_millis(200);

struct Outgoing {
    bytes: Vec<u8>,
    sent: Option<Instant>,
}

pub(super) struct Control<T> {
    pub inner: T,
    session: [u8; 16],
    player: usize,
    next_send: u64,
    next_receive: u64,
    sending: VecDeque<Vec<u8>>,
    send_offset: usize,
    outgoing: BTreeMap<u64, Outgoing>,
    incoming: BTreeMap<u64, Vec<u8>>,
    assembling: Vec<u8>,
    expected: Option<usize>,
    messages: VecDeque<Vec<u8>>,
    game: VecDeque<Vec<u8>>,
    ack_pending: bool,
}

impl<T: Transport> Control<T> {
    pub fn new(inner: T, session: [u8; 16], player: usize) -> Self {
        Self {
            inner,
            session,
            player,
            next_send: 0,
            next_receive: 0,
            sending: VecDeque::new(),
            send_offset: 0,
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            assembling: Vec::new(),
            expected: None,
            messages: VecDeque::new(),
            game: VecDeque::new(),
            ack_pending: false,
        }
    }

    pub fn send_message(&mut self, bytes: Vec<u8>) -> Result<()> {
        ensure!(
            bytes.len() <= super::setup::MAX_BUNDLE + 1 && self.sending.len() < 4,
            "netplay control send queue is full"
        );
        let mut framed = Vec::with_capacity(bytes.len() + 4);
        framed.extend((bytes.len() as u32).to_le_bytes());
        framed.extend(bytes);
        self.sending.push_back(framed);
        Ok(())
    }

    pub fn take_message(&mut self) -> Option<Vec<u8>> {
        self.messages.pop_front()
    }
    pub fn sending(&self) -> bool {
        !self.sending.is_empty() || !self.outgoing.is_empty()
    }
    pub fn has_game_packets(&self) -> bool {
        self.game.iter().any(|bytes| {
            super::wire::Packet::decode(bytes).is_some_and(|packet| packet.session == self.session)
        })
    }
    pub fn received_bytes(&self) -> usize {
        self.assembling.len()
    }

    pub fn poll(&mut self) -> Result<()> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Result<()> {
        if !self.inner.ready()? {
            return Ok(());
        }
        let mut buffer = [0; MAX_PACKET + 1];
        for _ in 0..128 {
            let Some(len) = self.inner.receive(&mut buffer)? else {
                break;
            };
            let Some(bytes) = buffer.get(..len) else {
                continue;
            };
            if !bytes.starts_with(MAGIC) {
                super::wire::Packet::check_version(bytes, &self.session)?;
                if self.game.len() < 64 && !bytes.is_empty() {
                    self.game.push_back(bytes.to_vec());
                }
                continue;
            }
            if bytes.len() < HEADER || bytes.len() > MAX_PACKET || bytes[5..21] != self.session {
                continue;
            }
            ensure!(
                bytes[4] == 1,
                "incompatible desktop setup protocol; use the same build"
            );
            ensure!(
                usize::from(bytes[21]) == 1 - self.player,
                "netplay peers must use opposite player roles"
            );
            let seq = u64::from_le_bytes(bytes[22..30].try_into()?);
            let ack = u64::from_le_bytes(bytes[30..38].try_into()?);
            let mask = u32::from_le_bytes(bytes[38..42].try_into()?);
            ensure!(ack <= self.next_send, "peer acknowledged unsent setup data");
            self.outgoing
                .retain(|&n, _| n >= ack && (n - ack >= WINDOW || mask & (1 << (n - ack)) == 0));
            if bytes[42] == 0 {
                ensure!(bytes.len() == HEADER, "invalid setup acknowledgement");
                continue;
            }
            ensure!(
                bytes[42] == 1 && bytes.len() > HEADER,
                "invalid setup data packet"
            );
            self.ack_pending = true;
            if seq < self.next_receive {
                continue;
            }
            ensure!(
                seq - self.next_receive < WINDOW,
                "setup data exceeds receive window"
            );
            if let Some(previous) = self.incoming.get(&seq) {
                ensure!(
                    previous.as_slice() == &bytes[HEADER..],
                    "peer changed pending setup data"
                );
            } else {
                self.incoming.insert(seq, bytes[HEADER..].to_vec());
            }
            while let Some(chunk) = self.incoming.remove(&self.next_receive) {
                self.next_receive = self
                    .next_receive
                    .checked_add(1)
                    .context("setup sequence exhausted")?;
                self.assemble(&chunk)?;
            }
        }
        if self.ack_pending && self.inner.send(&self.packet(0, &[]))? {
            self.ack_pending = false;
        }
        // Keep the window anchored at the oldest unacknowledged sequence,
        // including selectively acknowledged packets after a missing chunk.
        let base = self
            .outgoing
            .first_key_value()
            .map_or(self.next_send, |(&n, _)| n);
        while self.next_send - base < WINDOW {
            let Some(message) = self.sending.front() else {
                break;
            };
            let end = (self.send_offset + CHUNK).min(message.len());
            let chunk = message[self.send_offset..end].to_vec();
            self.outgoing.insert(
                self.next_send,
                Outgoing {
                    bytes: chunk,
                    sent: None,
                },
            );
            self.next_send = self
                .next_send
                .checked_add(1)
                .context("setup sequence exhausted")?;
            self.send_offset = end;
            if end == message.len() {
                self.sending.pop_front();
                self.send_offset = 0;
            }
        }
        let due: Vec<_> = self
            .outgoing
            .iter()
            .filter(|(_, packet)| {
                packet
                    .sent
                    .is_none_or(|sent| now.duration_since(sent) >= RESEND)
            })
            .map(|(&seq, _)| seq)
            .collect();
        for seq in due {
            let packet = self.packet(seq, &self.outgoing[&seq].bytes);
            if !self.inner.send(&packet)? {
                break;
            }
            self.outgoing.get_mut(&seq).unwrap().sent = Some(now);
        }
        Ok(())
    }

    fn packet(&self, sequence: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + payload.len());
        bytes.extend(MAGIC);
        bytes.push(1);
        bytes.extend(self.session);
        bytes.push(self.player as u8);
        bytes.extend(sequence.to_le_bytes());
        bytes.extend(self.next_receive.to_le_bytes());
        let mask = self
            .incoming
            .keys()
            .fold(0u32, |mask, n| mask | (1 << (n - self.next_receive)));
        bytes.extend(mask.to_le_bytes());
        bytes.push(u8::from(!payload.is_empty()));
        bytes.extend(payload);
        bytes
    }

    fn assemble(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let target = self.expected.unwrap_or(4);
            let take = (target - self.assembling.len()).min(bytes.len());
            self.assembling.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.assembling.len() != target {
                continue;
            }
            if self.expected.is_none() {
                let len = u32::from_le_bytes(self.assembling[..4].try_into()?) as usize;
                ensure!(
                    len <= super::setup::MAX_BUNDLE + 1 && len > 0,
                    "invalid setup message length"
                );
                self.assembling.clear();
                self.expected = Some(len);
            } else {
                ensure!(
                    self.messages.len() < 4,
                    "netplay control receive queue is full"
                );
                self.messages
                    .push_back(std::mem::take(&mut self.assembling));
                self.expected = None;
            }
        }
        Ok(())
    }
}

use anyhow::Context;

impl<T: Transport> Transport for Control<T> {
    fn route(&self) -> &'static str {
        self.inner.route()
    }
    fn ready(&mut self) -> Result<bool> {
        // A disconnect can follow the last input/ack in the same poll. Let
        // the timeline consume those packets before surfacing the close.
        if self.game.is_empty() {
            self.inner.ready()
        } else {
            Ok(true)
        }
    }
    fn receive(&mut self, buffer: &mut [u8]) -> Result<Option<usize>> {
        let Some(bytes) = self.game.pop_front() else {
            return Ok(None);
        };
        ensure!(bytes.len() <= buffer.len(), "input packet too large");
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(Some(bytes.len()))
    }
    fn send(&mut self, packet: &[u8]) -> Result<bool> {
        self.inner.send(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netplay::PacketQueue;
    #[test]
    fn transfer_survives_loss_reordering_duplicates_and_backpressure() -> Result<()> {
        let mut a = Control::new(PacketQueue::default(), [3; 16], 0);
        let mut b = Control::new(PacketQueue::default(), [3; 16], 1);
        let payload: Vec<_> = (0..120_000).map(|n| (n % 251) as u8).collect();
        a.send_message(payload.clone())?;
        b.send_message(b"reply".to_vec())?;
        let mut now = Instant::now();
        let mut received = None;
        let mut reply = None;
        for tick in 0..500 {
            a.poll_at(now)?;
            b.poll_at(now)?;
            for reverse in [false, true] {
                let (from, to) = if reverse {
                    (&mut a.inner, &mut b.inner)
                } else {
                    (&mut b.inner, &mut a.inner)
                };
                let mut packets = Vec::new();
                while let Some(packet) = from.pop() {
                    packets.push(packet);
                }
                for (i, packet) in packets.into_iter().rev().enumerate() {
                    if (tick + i) % 7 != 0 {
                        to.push(&packet)?;
                        if i % 5 == 0 {
                            to.push(&packet)?;
                        }
                    }
                }
            }
            if let Some(message) = a.take_message() {
                ensure!(reply.is_none(), "duplicate delivery");
                reply = Some(message);
            }
            if let Some(message) = b.take_message() {
                ensure!(received.is_none(), "duplicate delivery");
                received = Some(message);
            }
            now += Duration::from_millis(50);
            if received.is_some() && reply.is_some() && !a.sending() && !b.sending() {
                break;
            }
        }
        assert_eq!(received, Some(payload));
        assert_eq!(reply, Some(b"reply".to_vec()));
        assert!(!a.sending() && !b.sending());
        Ok(())
    }
}
