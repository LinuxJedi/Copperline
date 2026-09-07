// SPDX-License-Identifier: GPL-3.0-or-later

//! Two-peer GGPO-style netplay. Only inputs and state digests cross the network.

mod rollback;
#[cfg(test)]
mod tests;
mod transport;
mod wire;
#[cfg(not(target_arch = "wasm32"))]
use transport::UdpTransport;
pub use transport::{PacketQueue, Transport};
/// Fixed wire layout, also exposed to browser glue for compatibility checks.
pub use wire::{HEADER as PACKET_HEADER, INPUT_RECORD, MAX_PACKET, VERSION as PROTOCOL_VERSION};
/// Default seed for a fitted clock in deterministic netplay sessions.
pub const RTC_SEED: u64 = 946684800;

use crate::emulator::Emulator;
use crate::timebase::{Duration, Instant};
use anyhow::{ensure, Context, Result};
use rollback::{Machine, Rollback};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[cfg(not(target_arch = "wasm32"))]
use std::net::SocketAddr;

/// Controller buttons, relative mouse motion and held keys for one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Input {
    /// Up, down, left, right, red, blue, play, rewind, forward, green, yellow.
    pub buttons: u16,
    pub keys: [u8; 16],
    pub mouse_dx: i16,
    pub mouse_dy: i16,
    /// Left, right and middle mouse buttons.
    pub mouse_buttons: u8,
}

impl Input {
    pub const BUTTONS: u16 = 0x7ff;

    pub fn add_mouse_delta(&mut self, dx: i32, dy: i32) {
        let add = |old: i16, delta: i32| {
            i32::from(old)
                .saturating_add(delta)
                .clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
        };
        self.mouse_dx = add(self.mouse_dx, dx);
        self.mouse_dy = add(self.mouse_dy, dy);
    }

    pub fn set_mouse_button(&mut self, button: u8, pressed: bool) {
        if button < 3 {
            let mask = 1 << button;
            self.mouse_buttons = (self.mouse_buttons & !mask) | (u8::from(pressed) << button);
        }
    }

    fn without_motion(mut self) -> Self {
        self.mouse_dx = 0;
        self.mouse_dy = 0;
        self
    }

    /// Direction switches, red/fire and blue/second button, in wire order.
    pub fn set_joystick(&mut self, held: [bool; 6]) {
        self.buttons = (self.buttons & !0x3f) | Self::pack_buttons(held);
    }

    /// Play, rewind, forward, green and yellow, in wire order.
    pub fn set_cd32_buttons(&mut self, held: [bool; 5]) {
        self.buttons = (self.buttons & 0x3f) | (Self::pack_buttons(held) << 6);
    }

    fn pack_buttons<const N: usize>(held: [bool; N]) -> u16 {
        held.into_iter()
            .enumerate()
            .fold(0, |bits, (bit, on)| bits | (u16::from(on) << bit))
    }

    pub fn set_key(&mut self, key: u8, pressed: bool) {
        if key >= 128 {
            return;
        }
        let mask = 1 << (key % 8);
        if pressed {
            self.keys[usize::from(key / 8)] |= mask;
        } else {
            self.keys[usize::from(key / 8)] &= !mask;
        }
    }

    fn merged_keys(inputs: [Self; 2]) -> [u8; 16] {
        std::array::from_fn(|i| inputs[0].keys[i] | inputs[1].keys[i])
    }
}

/// Decode the shared game identifier used by both CLI and GUI setup.
pub fn parse_session_id(code: &str) -> Result<[u8; 16]> {
    ensure!(code.len() == 32 && code.bytes().all(|b| b.is_ascii_hexdigit()),
        "Session code needs exactly 32 hexadecimal digits; create a new code or paste your peer's code");
    let mut session = [0; 16];
    for (i, byte) in session.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&code[i * 2..i * 2 + 2], 16)?;
    }
    Ok(session)
}

/// Negotiated timeline settings, shared by every transport.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Zero-based controller port owned by this peer.
    pub player: usize,
    pub session: [u8; 16],
    pub input_delay: u8,
    pub rollback_frames: u8,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.player < 2, "netplay player must be 1 or 2");
        ensure!(
            self.input_delay <= 6,
            "netplay input delay must be 0..6 frames"
        );
        ensure!(
            (1..=12).contains(&self.rollback_frames),
            "netplay rollback window must be 1..12 frames"
        );
        Ok(())
    }
}

/// Both peers specify each other's reachable UDP address and the same session ID.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct Options {
    pub bind: SocketAddr,
    pub peer: SocketAddr,
    /// Zero-based controller port owned by this peer.
    pub player: usize,
    pub session: [u8; 16],
    pub input_delay: u8,
    pub rollback_frames: u8,
}

#[cfg(not(target_arch = "wasm32"))]
impl Options {
    fn settings(&self) -> Settings {
        Settings {
            player: self.player,
            session: self.session,
            input_delay: self.input_delay,
            rollback_frames: self.rollback_frames,
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.settings().validate()?;
        ensure!(
            self.bind.is_ipv4() == self.peer.is_ipv4(),
            "netplay addresses must use the same IP family"
        );
        ensure!(
            self.peer.port() != 0
                && !self.peer.ip().is_unspecified()
                && !self.peer.ip().is_multicast(),
            "netplay peer must be a unicast address with a nonzero port"
        );
        Ok(())
    }
}

pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Validate static host dependencies before building or connecting a machine.
pub fn validate_config(cfg: &crate::config::Config) -> Result<()> {
    if let Some(reason) = cfg.runahead_machine_block_reason() {
        anyhow::bail!("netplay cannot use {reason}");
    }
    // The sampler attaches in the frontend after session construction; reject
    // it here, before fingerprinting or opening any parallel host device.
    ensure!(
        cfg.parallel.device == crate::config::ParallelDevice::None,
        "netplay requires the parallel port device to be none"
    );
    // Its rate-specific resamplers serialize from a randomized HashMap, so
    // equivalent boards cannot yet guarantee byte-identical checkpoints.
    ensure!(!cfg.toccata, "netplay cannot use the Toccata sound board");
    ensure!(
        !cfg.cpu_jit
            && cfg.emulation.power_on
            && !cfg.emulation.rewind
            && cfg.emulation.run_ahead_frames == 0,
        "netplay requires power on, interpreter execution, rewind off and run-ahead off"
    );
    ensure!(
        !cfg.emulation.warp_boot && cfg.emulation.warp_until.is_none(),
        "netplay cannot use warp boot"
    );
    ensure!(
        matches!(cfg.serial.mode, crate::config::SerialMode::Off),
        "netplay requires --serial off"
    );
    ensure!(
        cfg.floppy.bridges.iter().all(Option::is_none),
        "netplay cannot use physical floppy drives"
    );
    Ok(())
}

/// Apply the deterministic clock default before constructing a netplay machine.
pub fn prepare_config(cfg: &mut crate::config::Config) -> Result<()> {
    validate_config(cfg)?;
    if cfg.rtc_present && cfg.rtc_seed_unix.is_none() {
        cfg.rtc_seed_unix = Some(RTC_SEED);
        log::info!("netplay: guest clock starts at 2000-01-01 00:00:00 UTC");
    }
    Ok(())
}

/// A session is serviced on the emulation thread; socket I/O never blocks it.
#[cfg(not(target_arch = "wasm32"))]
pub type Session = Connection<UdpTransport>;

pub struct Connection<T: Transport> {
    transport: T,
    settings: Settings,
    identity: [u8; 32],
    rollback: Rollback,
    seen_peer: bool,
    connected: bool,
    started: Instant,
    last_received: Instant,
    last_sent: Option<Instant>,
    peer_hashes: BTreeMap<u64, [u8; 32]>,
    last_checked: u64,
    failure: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub connected: bool,
    pub frame: u64,
    pub confirmed_frame: u64,
    /// All local inputs below this frame have reached the peer.
    pub acknowledged_frame: u64,
    pub rollbacks: u64,
    pub replayed_frames: u64,
    pub checked_frame: u64,
}

impl Status {
    /// A capture may end this process, so both peers need its frame's inputs.
    pub fn ready_to_capture(&self) -> bool {
        self.connected
            && self.frame == self.confirmed_frame
            && self.frame <= self.acknowledged_frame
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Connection<UdpTransport> {
    pub fn new(options: Options, emu: &mut Emulator, cfg: &crate::config::Config) -> Result<Self> {
        options.validate()?;
        let settings = options.settings();
        let transport = UdpTransport::new(options)?;
        Self::with_transport(settings, transport, emu, cfg)
    }

    pub fn options(&self) -> &Options {
        &self.transport.options
    }
}

impl<T: Transport> Connection<T> {
    pub fn with_transport(
        settings: Settings,
        transport: T,
        emu: &mut Emulator,
        cfg: &crate::config::Config,
    ) -> Result<Self> {
        validate_config(cfg)?;
        ensure!(
            emu.bus().emulated_cck() == 0,
            "netplay must start before the machine runs"
        );
        settings.validate()?;
        // Paths are host metadata; normalize only after adopting the complete
        // images into memory so replay cannot reopen or overwrite local files.
        emu.bus_mut().floppy.prepare_netplay_images();
        if let Some(reason) = emu
            .bus()
            .runahead_host_block_reason()
            .or_else(|| emu.machine.runahead_debug_block_reason())
        {
            anyhow::bail!("netplay cannot use {reason}");
        }
        ensure!(
            !emu.time_travel_enabled(),
            "netplay cannot record reverse history"
        );
        ensure!(
            emu.bus().input.ports.iter().all(|p| matches!(
                p.device,
                crate::bus::PortDevice::Mouse
                    | crate::bus::PortDevice::Joystick
                    | crate::bus::PortDevice::Cd32Pad
            )),
            "netplay requires mouse, joystick or CD32 controllers on both ports"
        );
        let mut identity_hash = Sha256::new();
        identity_hash.update(env!("COPPERLINE_DISPLAY_VERSION").as_bytes());
        identity_hash.update(emu.netplay_snapshot()?);
        let identity = identity_hash.finalize().into();
        let rollback = Rollback::new(
            settings.player,
            settings.input_delay,
            settings.rollback_frames,
        );
        Ok(Self {
            transport,
            settings,
            identity,
            rollback,
            seen_peer: false,
            connected: false,
            started: Instant::now(),
            last_received: Instant::now(),
            last_sent: None,
            peer_hashes: BTreeMap::new(),
            last_checked: 0,
            failure: None,
        })
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn player(&self) -> usize {
        self.settings.player
    }

    pub fn status(&self) -> Status {
        Status {
            connected: self.connected,
            frame: self.rollback.current,
            confirmed_frame: self.rollback.confirmed,
            acknowledged_frame: self.rollback.acknowledged,
            rollbacks: self.rollback.rollbacks,
            replayed_frames: self.rollback.replayed_frames,
            checked_frame: self.last_checked,
        }
    }

    /// A transport-coordinated media change may only touch a fully confirmed
    /// boundary. No retained prediction can subsequently restore older media.
    pub fn confirmed_state_digest(&self, emu: &Emulator) -> Result<[u8; 32]> {
        ensure!(self.failure.is_none(), "netplay session has failed");
        ensure!(
            self.status().ready_to_capture(),
            "netplay frame is not confirmed"
        );
        Ok(digest(&emu.netplay_snapshot()?))
    }

    /// Poll, repair late input, and optionally advance a frame. `false` is a
    /// normal wait for handshake/input. Continue polling while waiting.
    pub fn step(&mut self, emu: &mut Emulator, mut input: Input, advance: bool) -> Result<bool> {
        self.step_local(emu, &mut input, advance)
    }

    /// Consume mouse motion only when a new local frame is sampled. Motion
    /// arriving during a handshake or a repeated stalled poll stays pending.
    pub fn step_local(
        &mut self,
        emu: &mut Emulator,
        input: &mut Input,
        advance: bool,
    ) -> Result<bool> {
        if let Some(error) = &self.failure {
            anyhow::bail!("{error}");
        }
        let result = self.step_inner(emu, input, advance);
        if let Err(error) = &result {
            self.failure = Some(format!("{error:#}"));
        }
        result
    }

    fn step_inner(&mut self, emu: &mut Emulator, input: &mut Input, advance: bool) -> Result<bool> {
        // A finite receive budget keeps window input responsive under a burst.
        let mut buffer = [0; wire::MAX_PACKET + 1];
        for _ in 0..64 {
            match self.transport.receive(&mut buffer) {
                Ok(Some(len)) => {
                    let Some(bytes) = buffer.get(..len) else {
                        continue;
                    };
                    wire::Packet::check_version(bytes, &self.settings.session)?;
                    let Some(packet) = wire::Packet::decode(bytes) else {
                        continue;
                    };
                    if packet.session != self.settings.session {
                        continue;
                    }
                    ensure!(packet.player == 1 - self.settings.player && packet.delay == self.settings.input_delay && packet.window == self.settings.rollback_frames,
                        "netplay settings differ: use opposite players and identical delay/rollback values");
                    ensure!(packet.identity == self.identity, "netplay initial machine mismatch: use the same build, ROM, disks, floppy sounds and deterministic machine settings");
                    self.seen_peer = true;
                    if packet.ready && !self.connected {
                        self.connected = true;
                        emu.reanchor_realtime_clock();
                        log::info!(
                            "netplay: connected; local controller port {}",
                            self.player() + 1
                        );
                    }
                    self.last_received = Instant::now();
                    self.rollback.acknowledge(packet.ack)?;
                    for (frame, input) in packet.inputs {
                        self.rollback.receive(frame, input)?;
                    }
                    if let Some((frame, hash)) = packet.checksum {
                        ensure!(
                            frame
                                <= self.rollback.current + u64::from(self.settings.input_delay) + 1,
                            "netplay checksum is too far in the future"
                        );
                        if frame > self.last_checked {
                            self.peer_hashes.insert(frame, hash);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(e).context("receiving netplay input"),
            }
        }
        ensure!(
            input.buttons & !Input::BUTTONS == 0 && input.mouse_buttons & !7 == 0,
            "invalid local netplay controller input"
        );
        let now = Instant::now();
        ensure!(
            if self.connected {
                now.duration_since(self.last_received) < Duration::from_secs(10)
            } else {
                now.duration_since(self.started) < Duration::from_secs(60)
            },
            "netplay peer timed out"
        );
        let mut stepped = false;
        // Stay below the signed wrap limit of the 8-bit JOYDAT counters.
        let sampled = Input {
            mouse_dx: input.mouse_dx.clamp(-100, 100),
            mouse_dy: input.mouse_dy.clamp(-100, 100),
            ..*input
        };
        if self.connected && advance {
            if self.rollback.submit_local(sampled) {
                input.mouse_dx -= sampled.mouse_dx;
                input.mouse_dy -= sampled.mouse_dy;
            }
            // Send sampled input before replay, emulation, or pacing can add
            // another frame of avoidable network latency.
            self.send_packet(true)?;
        }
        if self.connected {
            let mut machine = EmulatedMachine(emu);
            self.rollback.reconcile(&mut machine)?;
            if advance {
                stepped = self.rollback.advance(&mut machine, sampled)?;
            }
            for (&frame, expected) in &self.peer_hashes {
                if let Some(actual) = self.rollback.hashes.get(&frame) {
                    ensure!(
                        expected == actual,
                        "netplay desynchronized at confirmed frame {frame}"
                    );
                    self.last_checked = self.last_checked.max(frame);
                }
            }
            self.peer_hashes.retain(|f, _| *f > self.last_checked);
        }
        self.send_packet(!advance)?;
        Ok(stepped)
    }

    fn send_packet(&mut self, force: bool) -> Result<()> {
        let now = Instant::now();
        if force
            || self
                .last_sent
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(10))
        {
            let packet = wire::Packet {
                session: self.settings.session,
                identity: self.identity,
                player: self.player(),
                ready: self.seen_peer,
                delay: self.settings.input_delay,
                window: self.settings.rollback_frames,
                ack: self.rollback.received,
                inputs: self
                    .rollback
                    .local
                    .range(self.rollback.acknowledged..)
                    .map(|(&f, &i)| (f, i))
                    .collect(),
                checksum: self.rollback.hashes.last_key_value().map(|(&f, &h)| (f, h)),
            }
            .encode();
            if self
                .transport
                .send(&packet)
                .context("sending netplay input")?
            {
                self.last_sent = Some(now);
            }
        }
        Ok(())
    }
}

impl<T: Transport> Drop for Connection<T> {
    fn drop(&mut self) {
        let status = self.status();
        log::info!(
            "netplay: finished frames={} confirmed={} checked={} rollbacks={} replayed={}",
            status.frame,
            status.confirmed_frame,
            status.checked_frame,
            status.rollbacks,
            status.replayed_frames
        );
    }
}

struct EmulatedMachine<'a>(&'a mut Emulator);
impl Machine for EmulatedMachine<'_> {
    fn save(&self) -> Result<Vec<u8>> {
        self.0.netplay_snapshot()
    }
    fn load(&mut self, state: &[u8]) -> Result<()> {
        self.0.netplay_restore(state)
    }
    fn frame(&mut self, inputs: [Input; 2], previous_keys: [u8; 16], replay: bool) -> Result<()> {
        for (port, input) in inputs.iter().enumerate() {
            if self.0.bus().input.ports[port].device == crate::bus::PortDevice::Mouse {
                let hardware = &mut self.0.bus_mut().input;
                hardware.add_mouse_delta(
                    port,
                    i32::from(input.mouse_dx),
                    i32::from(input.mouse_dy),
                );
                for button in 0..3 {
                    hardware.set_mouse_button(
                        port,
                        button,
                        input.mouse_buttons & (1 << button) != 0,
                    );
                }
                continue;
            }
            let on = |bit: u32| input.buttons & (1u16 << bit) != 0u16;
            self.0
                .bus_mut()
                .input
                .set_joystick(port, on(0), on(1), on(2), on(3), on(4), on(5));
            self.0
                .bus_mut()
                .input
                .set_cd32_buttons(port, on(6), on(7), on(8), on(9), on(10));
        }
        let keys = Input::merged_keys(inputs);
        for key in 0..128u8 {
            let mask = 1 << (key % 8);
            let index = usize::from(key / 8);
            if (keys[index] ^ previous_keys[index]) & mask != 0 {
                self.0
                    .bus_mut()
                    .enqueue_key_event(key, keys[index] & mask != 0);
            }
        }
        self.0.step_netplay_frame(replay)
    }
}
