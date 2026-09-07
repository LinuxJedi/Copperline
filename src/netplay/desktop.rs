// SPDX-License-Identifier: GPL-3.0-or-later

use super::{
    control::Control,
    setup::{Bundle, Staged},
    transport::{NativeTransport, UdpTransport},
    *,
};
use crate::{
    config::Config,
    emulator::Emulator,
    timebase::{Duration, Instant},
};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Message {
    Hello { build: String },
    Offer { delay: u8, window: u8 },
    Verified { identity: [u8; 32] },
    Start,
    Swap { id: u64, event: SwapMessage },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_adopts_host_setup_and_both_peers_commit_insert_and_eject() -> Result<()> {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| -> Result<()> {
                let reserve = [
                    std::net::UdpSocket::bind("127.0.0.1:0")?,
                    std::net::UdpSocket::bind("127.0.0.1:0")?,
                ];
                let addresses = [reserve[0].local_addr()?, reserve[1].local_addr()?];
                drop(reserve);
                let mut machines = [
                    super::super::tests::emulator()?,
                    super::super::tests::emulator()?,
                ];
                let mut cfg = super::super::tests::safe_config()?;
                cfg.floppy_connected = [true; 4];
                prepare_config(&mut cfg)?;
                let mut guest_cfg = cfg.clone();
                guest_cfg.chip_ram_bytes *= 2;
                let options = |player| Options {
                    bind: addresses[player],
                    peer: addresses[1 - player],
                    player,
                    session: [17; 16],
                    input_delay: 2,
                    rollback_frames: 8,
                };
                let mut peers = [
                    Session::new(options(0), &mut machines[0], &cfg)?,
                    Session::new(options(1), &mut machines[1], &guest_cfg)?,
                ];
                let deadline = Instant::now() + Duration::from_secs(90);
                while !peers.iter().all(|p| p.status().connected) {
                    for n in 0..2 {
                        peers[n].step(&mut machines[n], Input::default(), false)?;
                    }
                    ensure!(Instant::now() < deadline, "setup did not connect");
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(
                    peers[1].take_config().unwrap().chip_ram_bytes,
                    cfg.chip_ram_bytes
                );
                assert_eq!(
                    machines[0].netplay_snapshot()?,
                    machines[1].netplay_snapshot()?
                );
                assert!(!peers[1].can_change_disk());
                assert!(machines.iter().all(|emu| !emu.paced()));
                let before = machines[0].netplay_snapshot()?;
                assert!(peers[0]
                    .change_disk(&machines[0], 0, vec![1, 2, 3], true)
                    .is_err());
                assert_eq!(before, machines[0].netplay_snapshot()?);
                for (drive, bytes) in
                    (0..4).flat_map(|drive| [(drive, vec![0; 901_120]), (drive, Vec::new())])
                {
                    let inserted = !bytes.is_empty();
                    peers[0].change_disk(&machines[0], drive, bytes, inserted)?;
                    while peers.iter().any(|p| p.swap.is_some()) || !peers[0].can_change_disk() {
                        for n in 0..2 {
                            peers[n].step(&mut machines[n], Input::default(), true)?;
                        }
                        ensure!(Instant::now() < deadline, "disk change did not finish");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // Stop both on a common frame before comparing full state.
                    let target = peers.iter().map(|p| p.status().frame).max().unwrap() + 2;
                    while !peers
                        .iter()
                        .all(|p| p.status().frame == target && p.ready_to_capture())
                    {
                        for n in 0..2 {
                            let advance = peers[n].status().frame < target;
                            peers[n].step(&mut machines[n], Input::default(), advance)?;
                        }
                        ensure!(Instant::now() < deadline, "confirmation did not finish");
                    }
                    assert_eq!(machines[0].bus().floppy.disk_inserted(drive), inserted);
                    assert_eq!(machines[1].bus().floppy.disk_inserted(drive), inserted);
                    assert_eq!(
                        machines[0].netplay_snapshot()?,
                        machines[1].netplay_snapshot()?
                    );
                }
                Ok(())
            })?
            .join()
            .unwrap()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum SwapMessage {
    Begin {
        drive: usize,
        size: usize,
        hash: [u8; 32],
        writable: bool,
    },
    Held {
        frame: u64,
    },
    Target {
        frame: u64,
    },
    Ready {
        hash: [u8; 32],
    },
    Prepared,
    Apply,
    Applied {
        hash: [u8; 32],
    },
    Resume,
}

#[derive(PartialEq, Eq)]
enum SwapPhase {
    HostHeld,
    HostReady,
    HostPrepared,
    HostApplied,
    GuestTarget,
    GuestReady,
    GuestBytes,
    GuestApply,
    GuestResume,
}

struct Swap {
    phase: SwapPhase,
    stop: u64,
    drive: usize,
    writable: bool,
    size: usize,
    hash: [u8; 32],
    bytes: Option<Vec<u8>>,
    peer_digest: Option<[u8; 32]>,
    started: Instant,
}

#[derive(PartialEq, Eq)]
enum Phase {
    HostHello,
    GuestOffer,
    GuestBundle,
    HostVerified,
    GuestStart,
    Running,
}

/// Desktop setup and media coordination around the shared rollback protocol.
pub struct Session {
    pub(super) connection: Connection<Control<NativeTransport>>,
    options: ConnectionOptions,
    phase: Phase,
    bundle: Option<Vec<u8>>,
    directory: Option<tempfile::TempDir>,
    changed_config: Option<Config>,
    started: Instant,
    progress: Option<String>,
    last_progress: Instant,
    failure: Option<String>,
    swap: Option<Swap>,
    swap_id: u64,
}

impl Session {
    pub fn new(
        options: impl Into<ConnectionOptions>,
        emu: &mut Emulator,
        cfg: &Config,
    ) -> Result<Self> {
        let options = options.into();
        let settings = options.settings();
        settings.validate()?;
        validate_config(cfg)?;
        ensure!(
            emu.bus().emulated_cck() == 0,
            "netplay must start before the machine runs"
        );
        // Finish every fallible preparation before replacing the live machine
        // or moving its output sink. The host also uses the transmitted setup.
        let staged = if settings.player == 0 {
            let bundle = Bundle::capture(cfg, emu)?;
            Some((bundle.stage()?, bundle.encode()?))
        } else {
            None
        };
        let transport = match options.clone() {
            ConnectionOptions::Direct(options) => {
                options.validate()?;
                NativeTransport::Udp(UdpTransport::new(options)?)
            }
            #[cfg(feature = "netplay-internet")]
            ConnectionOptions::Internet(options) => {
                NativeTransport::Internet(Box::new(internet::InternetTransport::new(*options)?))
            }
        };
        let control = Control::new(transport, settings.session, settings.player);
        let (connection, directory, bundle, changed_config) =
            if let Some((mut staged, bytes)) = staged {
                staged.emu.set_paced(emu.paced());
                let connection = Connection::with_transport(
                    settings.clone(),
                    control,
                    &mut staged.emu,
                    &staged.cfg,
                )?;
                std::mem::swap(
                    &mut staged.emu.bus_mut().paula.audio,
                    &mut emu.bus_mut().paula.audio,
                );
                *emu = *staged.emu;
                (
                    connection,
                    Some(staged.directory),
                    Some(bytes),
                    Some(staged.cfg),
                )
            } else {
                (
                    Connection::with_transport(settings.clone(), control, emu, cfg)?,
                    None,
                    None,
                    None,
                )
            };
        let now = Instant::now();
        let mut session = Self {
            connection,
            options,
            phase: if settings.player == 0 {
                Phase::HostHello
            } else {
                Phase::GuestOffer
            },
            directory,
            bundle,
            changed_config,
            started: now,
            progress: Some("Waiting for the other player...".into()),
            last_progress: now,
            failure: None,
            swap: None,
            swap_id: 0,
        };
        if settings.player == 1 {
            session.send(Message::Hello {
                build: env!("COPPERLINE_DISPLAY_VERSION").into(),
            })?;
        }
        Ok(session)
    }

    pub fn options(&self) -> ConnectionOptions {
        self.options.clone()
    }
    pub fn player(&self) -> usize {
        self.connection.player()
    }
    pub fn status(&self) -> Status {
        self.connection.status()
    }
    pub fn route(&self) -> &'static str {
        self.connection.route()
    }
    pub fn confirmed_state_digest(&self, emu: &Emulator) -> Result<[u8; 32]> {
        self.connection.confirmed_state_digest(emu)
    }
    pub fn take_config(&mut self) -> Option<Config> {
        self.changed_config.take()
    }
    pub fn take_progress(&mut self) -> Option<String> {
        self.progress.take()
    }
    pub fn ready_to_capture(&self) -> bool {
        self.swap.is_none()
            && !self.connection.transport.sending()
            && self.status().ready_to_capture()
    }
    pub fn can_change_disk(&self) -> bool {
        self.player() == 0
            && self.phase == Phase::Running
            && self.status().connected
            && self.swap.is_none()
            && !self.connection.transport.sending()
    }

    /// Queue one host-controlled insertion or eject. Decode the local image
    /// before stopping the game, so a bad selection leaves play untouched.
    pub fn change_disk(
        &mut self,
        emu: &Emulator,
        drive: usize,
        bytes: Vec<u8>,
        writable: bool,
    ) -> Result<()> {
        ensure!(
            self.can_change_disk(),
            "wait for the host's connected, idle netplay session"
        );
        Self::validate_disk(emu, drive, &bytes, writable)?;
        let size = bytes.len();
        let hash = digest(&bytes);
        self.swap_id = self
            .swap_id
            .checked_add(1)
            .context("disk change identifier exhausted")?;
        self.swap = Some(Swap {
            phase: SwapPhase::HostHeld,
            stop: self.status().frame,
            drive,
            writable,
            size,
            hash,
            bytes: Some(bytes),
            peer_digest: None,
            started: Instant::now(),
        });
        self.swap_send(SwapMessage::Begin {
            drive,
            size,
            hash,
            writable,
        })?;
        self.progress = Some(format!("Pausing both players for DF{drive}..."));
        Ok(())
    }

    fn validate_disk(emu: &Emulator, drive: usize, bytes: &[u8], writable: bool) -> Result<()> {
        ensure!(
            drive < 4 && emu.bus().floppy.drive_connected(drive),
            "floppy drive is not connected"
        );
        ensure!(
            bytes.len() <= setup::FLOPPY_LIMIT && (!bytes.is_empty() || !writable),
            "invalid replacement disk size or write protection"
        );
        if !bytes.is_empty() {
            crate::floppy::FloppyController::default().insert_memory_disk_image_bytes_with_limit(
                0,
                bytes.to_vec(),
                "replacement".into(),
                !writable,
                setup::FLOPPY_LIMIT,
            )?;
        }
        Ok(())
    }

    fn swap_send(&mut self, event: SwapMessage) -> Result<()> {
        self.send(Message::Swap {
            id: self.swap_id,
            event,
        })
    }

    fn swap_message(&mut self, emu: &mut Emulator, id: u64, event: SwapMessage) -> Result<()> {
        if let SwapMessage::Begin {
            drive,
            size,
            hash,
            writable,
        } = event
        {
            ensure!(
                self.player() == 1
                    && self.status().connected
                    && self.swap.is_none()
                    && self.swap_id.checked_add(1) == Some(id),
                "unexpected disk change request"
            );
            ensure!(
                drive < 4
                    && emu.bus().floppy.drive_connected(drive)
                    && size <= setup::FLOPPY_LIMIT
                    && (size > 0 || !writable),
                "invalid disk change description"
            );
            self.swap_id = id;
            let frame = self.status().frame;
            self.swap = Some(Swap {
                phase: SwapPhase::GuestTarget,
                stop: frame,
                drive,
                size,
                hash,
                writable,
                bytes: None,
                peer_digest: None,
                started: Instant::now(),
            });
            self.progress = Some(format!("Host is changing DF{drive}..."));
            return self.swap_send(SwapMessage::Held { frame });
        }
        ensure!(id == self.swap_id, "unexpected disk change identifier");
        let frame = self.status().frame;
        let swap = self.swap.as_mut().context("no disk change in progress")?;
        match event {
            SwapMessage::Held { frame: peer } if swap.phase == SwapPhase::HostHeld => {
                ensure!(peer.abs_diff(frame) <= 32, "invalid peer disk change frame");
                let target = peer.max(frame);
                swap.stop = target;
                swap.phase = SwapPhase::HostReady;
                self.swap_send(SwapMessage::Target { frame: target })?;
            }
            SwapMessage::Target { frame: target } if swap.phase == SwapPhase::GuestTarget => {
                ensure!(
                    target >= frame && target - frame <= 32,
                    "invalid disk change target"
                );
                swap.stop = target;
                swap.phase = SwapPhase::GuestReady;
            }
            SwapMessage::Ready { hash } if swap.phase == SwapPhase::HostReady => {
                swap.peer_digest = Some(hash);
            }
            SwapMessage::Prepared if swap.phase == SwapPhase::HostPrepared => {
                self.apply_disk(emu)?;
                self.swap.as_mut().unwrap().phase = SwapPhase::HostApplied;
                self.swap_send(SwapMessage::Apply)?;
            }
            SwapMessage::Apply if swap.phase == SwapPhase::GuestApply => {
                self.apply_disk(emu)?;
                self.swap.as_mut().unwrap().phase = SwapPhase::GuestResume;
                self.swap_send(SwapMessage::Applied {
                    hash: self.confirmed_state_digest(emu)?,
                })?;
            }
            SwapMessage::Applied { hash } if swap.phase == SwapPhase::HostApplied => {
                ensure!(
                    self.confirmed_state_digest(emu)? == hash,
                    "players differ after the disk change"
                );
                self.swap_send(SwapMessage::Resume)?;
                self.finish_swap();
            }
            SwapMessage::Resume if swap.phase == SwapPhase::GuestResume => {
                self.finish_swap();
            }
            _ => anyhow::bail!("unexpected disk change phase"),
        }
        Ok(())
    }

    fn apply_disk(&mut self, emu: &mut Emulator) -> Result<()> {
        let status = self.status();
        let swap = self.swap.as_mut().context("no disk change in progress")?;
        ensure!(
            status.ready_to_capture() && status.frame == swap.stop,
            "disk change is not at a confirmed boundary"
        );
        let bytes = swap
            .bytes
            .take()
            .context("replacement disk is not verified")?;
        if bytes.is_empty() {
            emu.bus_mut().floppy.eject_disk_image(swap.drive)?;
        } else {
            emu.bus_mut()
                .floppy
                .insert_memory_disk_image_bytes_with_limit(
                    swap.drive,
                    bytes,
                    format!("netplay-df{}", swap.drive).into(),
                    !swap.writable,
                    setup::FLOPPY_LIMIT,
                )?;
        }
        Ok(())
    }

    fn finish_swap(&mut self) {
        let swap = self.swap.take().unwrap();
        self.progress = Some(format!(
            "DF{} {} on both players",
            swap.drive,
            if swap.size == 0 {
                "ejected"
            } else {
                "inserted"
            }
        ));
    }
    pub fn step(&mut self, emu: &mut Emulator, input: Input, advance: bool) -> Result<bool> {
        self.step_local(emu, &mut input.into(), advance)
    }

    pub fn step_local(
        &mut self,
        emu: &mut Emulator,
        input: &mut LocalInput,
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

    fn send(&mut self, message: Message) -> Result<()> {
        let mut bytes = vec![1];
        bytes.extend(serde_json::to_vec(&message)?);
        self.connection.transport.send_message(bytes)
    }

    fn step_inner(
        &mut self,
        emu: &mut Emulator,
        input: &mut LocalInput,
        advance: bool,
    ) -> Result<bool> {
        self.connection.transport.poll()?;
        ensure!(
            !matches!(self.phase, Phase::HostHello | Phase::GuestOffer)
                || !self.connection.transport.has_game_packets(),
            "peer does not support desktop setup transfer; use the same Copperline build"
        );
        while let Some(bytes) = self.connection.transport.take_message() {
            if bytes.first() == Some(&3) {
                let swap = self.swap.as_mut().context("unexpected replacement disk")?;
                ensure!(
                    swap.phase == SwapPhase::GuestBytes
                        && bytes.len() == swap.size + 1
                        && digest(&bytes[1..]) == swap.hash,
                    "invalid replacement disk transfer"
                );
                Self::validate_disk(emu, swap.drive, &bytes[1..], swap.writable)?;
                swap.bytes = Some(bytes[1..].to_vec());
                swap.phase = SwapPhase::GuestApply;
                self.swap_send(SwapMessage::Prepared)?;
                continue;
            }
            if self.phase == Phase::GuestBundle {
                ensure!(bytes.first() == Some(&2), "expected host setup bundle");
                let Staged {
                    emu: mut received,
                    cfg,
                    directory,
                } = Bundle::decode(&bytes[1..])?.stage()?;
                // Reuse the same validation as initial session construction.
                self.connection.identity =
                    initial_identity(&self.connection.settings, &mut received, &cfg)?;
                self.connection.rollback = Rollback::new(
                    self.player(),
                    self.connection.settings.input_delay,
                    self.connection.settings.rollback_frames,
                );
                received.set_paced(emu.paced());
                std::mem::swap(
                    &mut received.bus_mut().paula.audio,
                    &mut emu.bus_mut().paula.audio,
                );
                *emu = *received;
                *input = LocalInput::default();
                self.directory = Some(directory);
                self.changed_config = Some(cfg);
                self.send(Message::Verified {
                    identity: self.connection.identity,
                })?;
                self.phase = Phase::GuestStart;
                self.progress = Some("Host setup verified; waiting to start...".into());
                continue;
            }
            ensure!(
                bytes.first() == Some(&1) && bytes.len() <= 2048,
                "invalid netplay setup message"
            );
            let message: Message = serde_json::from_slice(&bytes[1..])?;
            match message {
                Message::Hello { build } if self.phase == Phase::HostHello => {
                    ensure!(
                        build == env!("COPPERLINE_DISPLAY_VERSION"),
                        "netplay requires the same Copperline build on both peers"
                    );
                    self.send(Message::Offer {
                        delay: self.connection.settings.input_delay,
                        window: self.connection.settings.rollback_frames,
                    })?;
                    let mut bytes = vec![2];
                    bytes.extend(self.bundle.take().unwrap());
                    self.connection.transport.send_message(bytes)?;
                    self.phase = Phase::HostVerified;
                    self.progress = Some("Sending machine configuration and game files...".into());
                }
                Message::Offer { delay, window } if self.phase == Phase::GuestOffer => {
                    let mut settings = self.connection.settings.clone();
                    settings.input_delay = delay;
                    settings.rollback_frames = window;
                    settings.validate()?;
                    self.connection.settings = settings;
                    self.phase = Phase::GuestBundle;
                    self.progress =
                        Some("Receiving machine configuration and game files...".into());
                }
                Message::Verified { identity } if self.phase == Phase::HostVerified => {
                    ensure!(
                        identity == self.connection.identity,
                        "received setup produced a different machine"
                    );
                    self.send(Message::Start)?;
                    self.phase = Phase::Running;
                }
                Message::Start if self.phase == Phase::GuestStart => {
                    self.phase = Phase::Running;
                }
                Message::Swap { id, event } if self.phase == Phase::Running => {
                    self.swap_message(emu, id, event)?;
                }
                _ => anyhow::bail!("unexpected netplay setup message"),
            }
        }
        if self.phase != Phase::Running {
            ensure!(
                self.started.elapsed() < Duration::from_secs(15 * 60),
                "netplay setup timed out"
            );
            self.connection.started = Instant::now();
            if self.phase == Phase::GuestBundle
                && self.last_progress.elapsed() > Duration::from_secs(1)
            {
                self.progress = Some(format!(
                    "Receiving game files: {} KiB",
                    self.connection.transport.received_bytes() / 1024
                ));
                self.last_progress = Instant::now();
            }
            return Ok(false);
        }
        let advance = advance
            && self
                .swap
                .as_ref()
                .is_none_or(|swap| self.status().frame < swap.stop);
        let stepped = self.connection.step_local(emu, input, advance)?;
        let status = self.status();
        if let Some(swap) = &self.swap {
            ensure!(
                swap.started.elapsed() < Duration::from_secs(180),
                "disk change timed out"
            );
            if status.frame == swap.stop && status.ready_to_capture() {
                let own = self.confirmed_state_digest(emu)?;
                if swap.phase == SwapPhase::GuestReady {
                    self.swap.as_mut().unwrap().phase = SwapPhase::GuestBytes;
                    self.swap_send(SwapMessage::Ready { hash: own })?;
                } else if swap.phase == SwapPhase::HostReady {
                    if let Some(peer) = swap.peer_digest {
                        ensure!(peer == own, "players differ before the disk change");
                        let swap = self.swap.as_mut().unwrap();
                        let mut bytes = vec![3];
                        bytes.extend(swap.bytes.as_ref().unwrap());
                        swap.phase = SwapPhase::HostPrepared;
                        self.connection.transport.send_message(bytes)?;
                    }
                }
            }
        }
        Ok(stepped)
    }
}
