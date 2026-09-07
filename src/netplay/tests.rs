// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use std::net::UdpSocket;

#[derive(Default)]
struct ToyMachine {
    state: u64,
    audible: usize,
}
impl Machine for ToyMachine {
    fn save(&self) -> Result<Vec<u8>> {
        Ok(self.state.to_le_bytes().to_vec())
    }
    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        self.state = u64::from_le_bytes(bytes.try_into().unwrap());
        Ok(())
    }
    fn frame(&mut self, input: [Input; 2], previous: [u8; 16], replay: bool) -> Result<()> {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(u64::from(input[0].buttons) + 100 * u64::from(input[1].buttons));
        for (i, key) in Input::merged_keys(input).iter().enumerate() {
            self.state = self.state.wrapping_add(u64::from(key ^ previous[i]));
        }
        if !replay {
            self.audible += 1;
        }
        Ok(())
    }
}

fn input(frame: u64, player: u64) -> Input {
    let mut i = Input {
        buttons: ((frame * (player + 3) / 7) % 2048) as u16,
        mouse_dx: (frame % 19) as i16 - 9,
        mouse_dy: 11 - ((frame + player) % 23) as i16,
        mouse_buttons: ((frame / 3 + player) % 8) as u8,
        ..Default::default()
    };
    i.set_key(0x40, (frame + player) % 11 < 3);
    i
}

#[test]
fn late_reordered_and_duplicate_input_replays_to_the_baseline() -> Result<()> {
    for delay in [0, 2, 6] {
        let mut baseline = ToyMachine::default();
        let mut predicted = ToyMachine::default();
        let mut rb = Rollback::new(0, delay, 8);
        let mut previous = [0; 16];
        for f in 0..240 {
            // Delay alternating packets by 5 frames; deliver newest first and
            // duplicate it. Missing input forces corrections to prediction chains.
            if f >= 5 && f % 6 == 5 {
                for remote in (f - 5..=f).rev() {
                    let value = if remote < u64::from(delay) {
                        Input::default()
                    } else {
                        input(remote - u64::from(delay), 1)
                    };
                    rb.receive(remote, value)?;
                    rb.receive(remote, value)?;
                }
            }
            rb.acknowledged = f + u64::from(delay);
            rb.reconcile(&mut predicted)?;
            assert!(rb.advance(&mut predicted, input(f, 0))?);
            let pair = if f < u64::from(delay) {
                [Input::default(); 2]
            } else {
                [
                    input(f - u64::from(delay), 0),
                    input(f - u64::from(delay), 1),
                ]
            };
            baseline.frame(pair, previous, false)?;
            previous = Input::merged_keys(pair);
        }
        for f in 235..240 {
            rb.receive(f, input(f - u64::from(delay), 1))?;
        }
        rb.reconcile(&mut predicted)?;
        assert_eq!(predicted.state, baseline.state, "delay {delay}");
        assert_eq!(
            predicted.audible, 240,
            "replay must not emit duplicate audio"
        );
        assert!(rb.rollbacks > 0);
        assert_eq!(rb.confirmed, 240);
        assert_eq!(rb.hashes[&240], digest(&baseline.save()?));
    }
    Ok(())
}

#[test]
fn prediction_window_stalls_then_recovers_without_resampling_input() -> Result<()> {
    let mut rb = Rollback::new(0, 0, 3);
    let mut machine = ToyMachine::default();
    for f in 0..3 {
        assert!(rb.advance(&mut machine, input(f, 0))?);
    }
    let original = input(3, 0);
    assert!(!rb.advance(&mut machine, original)?);
    assert!(!rb.advance(&mut machine, input(99, 0))?);
    for f in 0..4 {
        rb.receive(f, input(f, 1))?;
    }
    rb.acknowledge(4)?;
    rb.reconcile(&mut machine)?;
    assert_eq!(rb.local[&3], original);
    assert!(rb.advance(&mut machine, input(99, 0))?);
    Ok(())
}

#[test]
fn delayed_input_from_a_faster_peer_stays_within_the_receive_horizon() -> Result<()> {
    for delay in [0, 2, 6] {
        for window in [1, 8, 12] {
            let mut slow = Rollback::new(0, delay, window);
            let mut fast = Rollback::new(1, delay, window);
            let mut machine = ToyMachine::default();
            slow.submit_local(input(0, 0));
            fast.receive(u64::from(delay), input(0, 0))?;
            loop {
                let advanced = fast.advance(&mut machine, input(fast.current, 1))?;
                // The slow peer polls and acknowledges input while its machine
                // remains at frame zero, as during expensive rendering or I/O.
                for (&frame, &value) in &fast.local {
                    slow.receive(frame, value)?;
                }
                fast.acknowledge(slow.received)?;
                if !advanced {
                    break;
                }
            }
            assert_eq!(fast.current, u64::from(delay + window) + 1);
            let last = fast.current + u64::from(delay);
            assert_eq!(slow.received, last + 1);
            assert!(slow.receive(last + 1, Input::default()).is_err());
        }
    }
    Ok(())
}

#[test]
fn invalid_input_and_ack_are_rejected() -> Result<()> {
    let mut rb = Rollback::new(0, 0, 8);
    rb.receive(0, input(0, 1))?;
    assert!(rb.receive(0, input(999, 1)).is_err());
    assert!(rb.receive(u64::MAX, Input::default()).is_err());
    assert!(rb.acknowledge(100).is_err());
    Ok(())
}

#[test]
fn browser_packet_queues_bound_bursts_and_preserve_recent_retransmissions() -> Result<()> {
    let mut queue = PacketQueue::default();
    for byte in 0..100 {
        queue.push(&[byte])?;
    }
    let mut buffer = [0; wire::MAX_PACKET + 1];
    for expected in 36..100 {
        assert_eq!(queue.receive(&mut buffer)?, Some(1));
        assert_eq!(buffer[0], expected);
    }
    assert_eq!(queue.receive(&mut buffer)?, None);
    assert!(queue.push(&vec![0; wire::MAX_PACKET + 1]).is_err());
    assert!(queue.send(&vec![0; wire::MAX_PACKET + 1]).is_err());
    queue.push(&[1, 2])?;
    assert!(queue.receive(&mut [0]).is_err());
    for _ in 0..64 {
        assert!(queue.send(&[1, 2])?);
    }
    assert!(!queue.send(&[3])?);
    assert_eq!(queue.pop(), Some(vec![1, 2]));
    assert!(queue.send(&[3])?);
    Ok(())
}

#[test]
fn wire_round_trip_and_bounded_rejection() {
    let packet = wire::Packet {
        session: [1; 16],
        identity: [2; 32],
        player: 1,
        ready: true,
        delay: 2,
        window: 8,
        ack: 3,
        inputs: vec![(3, input(3, 1)), (4, input(4, 1))],
        checksum: Some((60, [3; 32])),
    };
    let bytes = packet.encode();
    let mut full = packet.clone();
    full.inputs = (0..wire::MAX_INPUTS as u64)
        .map(|frame| {
            (
                frame,
                Input {
                    mouse_dx: i16::MIN,
                    mouse_dy: i16::MAX,
                    mouse_buttons: 7,
                    ..Default::default()
                },
            )
        })
        .collect();
    let maximum = full.encode();
    assert_eq!(maximum.len(), wire::MAX_PACKET);
    assert!(maximum.len() <= 1200);
    assert_eq!(wire::Packet::decode(&maximum), Some(full));
    let mut invalid_mouse = bytes.clone();
    invalid_mouse[wire::HEADER + wire::INPUT_RECORD - 1] = 8;
    assert!(wire::Packet::decode(&invalid_mouse).is_none());
    assert_eq!(wire::Packet::decode(&bytes), Some(packet));
    for end in 0..bytes.len() {
        assert!(wire::Packet::decode(&bytes[..end]).is_none());
    }
    let mut oversized = bytes.clone();
    oversized.push(0);
    assert!(wire::Packet::decode(&oversized).is_none());
    for length in [0, 1, 117, 1200, 65535] {
        assert!(wire::Packet::decode(&vec![0xff; length]).is_none());
    }
}

fn emulator() -> Result<Emulator> {
    use crate::{
        audio::NullSink,
        bus::{Bus, PortDevice},
        chipset::paula::Paula,
        config::{CpuModel, PacingBudget},
        floppy::FloppyController,
        memory::{Memory, ROM_BASE, ROM_SIZE},
        serial::NullSerialSink,
    };
    let mut rom = vec![0; ROM_SIZE];
    rom[..4].copy_from_slice(&0x0007fffeu32.to_be_bytes());
    rom[4..8].copy_from_slice(&(ROM_BASE as u32 + 8).to_be_bytes());
    // Copy both JOYDAT registers and CIA fire lines to RAM; also drive COLOR00
    // from port 1 so the rendered output depends on the predicted input.
    let program: [u16; 23] = [
        0x33f9, 0x00df, 0xf00a, 0, 0x0180, 0x33f9, 0x00df, 0xf00c, 0, 0x0182, 0x13f9, 0x00bf,
        0xe001, 0, 0x0184, 0x33f9, 0x00df, 0xf00a, 0x00df, 0xf180, 0x52b8, 0x0188, 0x60d2,
    ];
    for (n, word) in program.iter().enumerate() {
        rom[8 + n * 2..10 + n * 2].copy_from_slice(&word.to_be_bytes());
    }
    let mut chip_ram = vec![0; 512 * 1024];
    chip_ram[..8].copy_from_slice(&rom[..8]);
    let mut bus = Bus::new(
        Memory {
            chip_ram,
            slow_ram: vec![],
            mb_ram: vec![],
            accel_ram: vec![],
            rom,
            overlay: false,
            zorro: Default::default(),
            extended_rom: vec![],
            extended_rom_base: 0,
            wcs: vec![],
            wcs_write_protected: false,
        },
        Paula::new(Box::new(NullSerialSink), Box::new(NullSink)),
        FloppyController::default(),
    );
    bus.rtc.set_seed(Some(946684800), false);
    bus.input.set_port_device(0, PortDevice::Joystick);
    bus.input.set_port_device(1, PortDevice::Joystick);
    Emulator::new(
        bus,
        CpuModel::M68000,
        false,
        Default::default(),
        PacingBudget::Cycles,
        2,
        false,
    )
}

#[test]
fn emulator_replay_matches_uninterrupted_machine_bytes() -> Result<()> {
    use crate::bus::PortDevice::{Cd32Pad, Joystick, Mouse};
    for devices in [[Joystick; 2], [Cd32Pad; 2], [Mouse; 2], [Mouse, Cd32Pad]] {
        replay_matches_devices(devices)?;
    }
    Ok(())
}

fn replay_matches_devices(devices: [crate::bus::PortDevice; 2]) -> Result<()> {
    let mut baseline = emulator()?;
    let mut predicted = emulator()?;
    for emu in [&mut baseline, &mut predicted] {
        for (port, device) in devices.into_iter().enumerate() {
            emu.bus_mut().input.set_port_device(port, device);
        }
    }
    let mut rb = Rollback::new(0, 0, 8);
    let mut previous = [0; 16];
    for f in 0..60 {
        if f % 6 == 5 && f < 59 {
            for remote in (f - 5..=f).rev() {
                rb.receive(remote, input(remote, 1))?;
            }
        }
        rb.acknowledged = f;
        rb.reconcile(&mut EmulatedMachine(&mut predicted))?;
        assert!(rb.advance(&mut EmulatedMachine(&mut predicted), input(f, 0))?);
        let pair = [input(f, 0), input(f, 1)];
        EmulatedMachine(&mut baseline).frame(pair, previous, false)?;
        previous = Input::merged_keys(pair);
    }
    for f in 54..60 {
        rb.receive(f, input(f, 1))?;
    }
    rb.reconcile(&mut EmulatedMachine(&mut predicted))?;
    assert!(rb.rollbacks > 1);
    assert_ne!(
        &baseline.bus().mem.chip_ram[0x188..0x18c],
        &[0; 4],
        "the guest workload must execute"
    );
    let mut expected = vec![0; crate::video::MAX_CANVAS_PIXELS];
    let mut actual = expected.clone();
    crate::video::bitplane::render_display_only(baseline.bus(), &mut expected);
    crate::video::bitplane::render_display_only(predicted.bus(), &mut actual);
    assert_eq!(actual, expected);
    assert_eq!(predicted.bus().mem.chip_ram, baseline.bus().mem.chip_ram);
    assert_eq!(
        digest(&predicted.runahead_snapshot()?),
        digest(&baseline.runahead_snapshot()?)
    );
    Ok(())
}

fn options(peer: SocketAddr, player: usize) -> Options {
    Options {
        bind: "127.0.0.1:0".parse().unwrap(),
        peer,
        player,
        session: [42; 16],
        input_delay: 0,
        rollback_frames: 8,
    }
}

#[test]
fn udp_peers_recover_loss_reordering_and_duplicates_and_confirm_checksums() -> Result<()> {
    for (delay, window) in [(0, 8), (2, 8), (6, 1)] {
        udp_pair_with_delay(delay, window)
            .with_context(|| format!("input delay {delay}, rollback window {window}"))?;
    }
    Ok(())
}

fn udp_pair_with_delay(delay: u8, window: u8) -> Result<()> {
    let proxy: [UdpSocket; 2] = [
        UdpSocket::bind("127.0.0.1:0")?,
        UdpSocket::bind("127.0.0.1:0")?,
    ];
    for socket in &proxy {
        socket.set_nonblocking(true)?;
    }
    let mut machines = [emulator()?, emulator()?];
    let session_options = |player: usize| -> Result<Options> {
        let mut options = options(proxy[player].local_addr()?, player);
        options.input_delay = delay;
        options.rollback_frames = window;
        Ok(options)
    };
    let mut sessions = [
        Session::new(session_options(0)?, &mut machines[0], &safe_config()?)?,
        Session::new(session_options(1)?, &mut machines[1], &safe_config()?)?,
    ];
    let destinations = [
        sessions[0].transport.socket.local_addr()?,
        sessions[1].transport.socket.local_addr()?,
    ];
    let mut queued = Vec::<(u64, usize, Vec<u8>)>::new();
    let mut packets = 0u64;
    for tick in 0..1500 {
        for player in 0..2 {
            let frame = sessions[player].status().frame;
            // A virtual transport tick is the retry clock for this test.
            sessions[player].last_sent = None;
            sessions[player].step(
                &mut machines[player],
                input(frame, player as u64),
                frame < 120 && (player != 0 || !(30..42).contains(&(tick % 90))),
            )?;
            let mut bytes = [0; wire::MAX_PACKET + 1];
            while let Ok((len, _)) = proxy[player].recv_from(&mut bytes) {
                packets += 1;
                if packets.is_multiple_of(7) {
                    continue;
                }
                let delay = packets % 5;
                queued.push((tick + delay, 1 - player, bytes[..len].to_vec()));
                if packets.is_multiple_of(11) {
                    queued.push((tick + delay + 2, 1 - player, bytes[..len].to_vec()));
                }
            }
        }
        // Newer packets with shorter delays overtake older ones.
        for (_, player, bytes) in queued.extract_if(.., |(due, _, _)| *due <= tick) {
            proxy[player].send_to(&bytes, destinations[player])?;
        }
        if sessions
            .iter()
            .all(|s| s.status().checked_frame == 120 && s.status().confirmed_frame == 120)
        {
            break;
        }
    }
    for session in &sessions {
        assert_eq!(session.status().confirmed_frame, 120);
        assert_eq!(session.status().checked_frame, 120);
        if delay == 0 {
            assert!(session.status().rollbacks > 0);
        }
        assert!(session.rollback.local.len() < 32);
        assert!(session.rollback.current - session.rollback.confirmed <= u64::from(window));
    }
    assert_eq!(
        digest(&machines[0].runahead_snapshot()?),
        digest(&machines[1].runahead_snapshot()?)
    );
    Ok(())
}

#[test]
fn mismatch_desync_and_disconnect_stop_the_session() -> Result<()> {
    let peer = UdpSocket::bind("127.0.0.1:0")?;
    let mut emu = emulator()?;
    let mut session = Session::new(options(peer.local_addr()?, 0), &mut emu, &safe_config()?)?;
    let mut packet = wire::Packet {
        session: [42; 16],
        identity: [0; 32],
        player: 1,
        ready: true,
        delay: 0,
        window: 8,
        ack: 0,
        inputs: vec![],
        checksum: None,
    };
    peer.send_to(&packet.encode(), session.transport.socket.local_addr()?)?;
    assert!(poll_error(&mut session, &mut emu)
        .to_string()
        .contains("mismatch"));
    assert!(
        session.step(&mut emu, Input::default(), true).is_err(),
        "failure stays latched"
    );
    let mut session = Session::new(options(peer.local_addr()?, 0), &mut emu, &safe_config()?)?;
    packet.identity = session.identity;
    session.connected = true;
    session.rollback.current = 60;
    session.rollback.confirmed = 60;
    session.rollback.received = 60;
    session.rollback.hashes.insert(60, [1; 32]);
    packet.checksum = Some((60, [2; 32]));
    peer.send_to(&packet.encode(), session.transport.socket.local_addr()?)?;
    assert!(poll_error(&mut session, &mut emu)
        .to_string()
        .contains("desynchronized"));
    let mut session = Session::new(options(peer.local_addr()?, 0), &mut emu, &safe_config()?)?;
    session.connected = true;
    session.last_received = Instant::now() - Duration::from_secs(11);
    assert!(session
        .step(&mut emu, Input::default(), false)
        .unwrap_err()
        .to_string()
        .contains("timed out"));
    Ok(())
}

fn safe_config() -> Result<crate::config::Config> {
    let mut cfg = crate::config::Config::try_from(crate::config::RawConfig::default())?;
    cfg.serial.mode = crate::config::SerialMode::Off;
    Ok(cfg)
}

#[test]
fn netplay_rejects_host_parallel_devices_and_noncanonical_toccata_state() -> Result<()> {
    use crate::config::ParallelDevice;
    let mut cfg = safe_config()?;
    validate_config(&cfg)?;
    for device in [ParallelDevice::Printer, ParallelDevice::Sampler] {
        cfg.parallel.device = device;
        assert!(prepare_config(&mut cfg)
            .unwrap_err()
            .to_string()
            .contains("parallel"));
    }
    cfg.parallel.device = ParallelDevice::None;
    cfg.toccata = true;
    assert!(prepare_config(&mut cfg)
        .unwrap_err()
        .to_string()
        .contains("Toccata"));
    cfg.toccata = false;
    prepare_config(&mut cfg)?;
    Ok(())
}

#[test]
fn capture_waits_for_the_peer_to_acknowledge_retransmitted_local_input() -> Result<()> {
    let peer = UdpSocket::bind("127.0.0.1:0")?;
    peer.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut emu = emulator()?;
    let mut session = Session::new(options(peer.local_addr()?, 0), &mut emu, &safe_config()?)?;
    let mut packet = wire::Packet {
        session: session.settings.session,
        identity: session.identity,
        player: 1,
        ready: true,
        delay: 0,
        window: 8,
        ack: 0,
        inputs: vec![(0, input(0, 1))],
        checksum: None,
    };
    peer.send_to(&packet.encode(), session.transport.socket.local_addr()?)?;
    for _ in 0..100 {
        if session.step(&mut emu, input(0, 0), true)? {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(session.status().frame, 1);
    assert_eq!(session.status().confirmed_frame, 1);
    assert_eq!(session.status().acknowledged_frame, 0);
    assert!(!session.status().ready_to_capture());
    // Discard the first local input datagram as an asymmetric packet loss.
    let mut bytes = [0; wire::MAX_PACKET + 1];
    let received_input = |bytes: &[u8]| {
        wire::Packet::decode(bytes).is_some_and(|packet| packet.inputs.contains(&(0, input(0, 0))))
    };
    loop {
        let len = peer.recv(&mut bytes)?;
        if received_input(&bytes[..len]) {
            break;
        }
    }
    let captured_state = emu.netplay_snapshot()?;
    session.step(&mut emu, Input::default(), false)?;
    let len = peer.recv(&mut bytes)?;
    assert!(
        received_input(&bytes[..len]),
        "capture polling retransmits input"
    );
    assert!(!session.status().ready_to_capture());
    packet.ack = 1;
    peer.send_to(&packet.encode(), session.transport.socket.local_addr()?)?;
    for _ in 0..100 {
        session.step(&mut emu, Input::default(), false)?;
        if session.status().ready_to_capture() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(session.status().ready_to_capture());
    assert_eq!(session.status().acknowledged_frame, 1);
    assert_eq!(emu.netplay_snapshot()?, captured_state);
    Ok(())
}

fn poll_error(session: &mut Session, emu: &mut Emulator) -> anyhow::Error {
    for _ in 0..100 {
        if let Err(error) = session.step(emu, Input::default(), false) {
            return error;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("expected peer error was not delivered");
}

#[test]
fn netplay_snapshot_preserves_the_completed_frame_and_runtime_latches() -> Result<()> {
    let mut emu = emulator()?;
    EmulatedMachine(&mut emu).frame([input(30, 0), input(30, 1)], [0; 16], false)?;
    let before = emu.netplay_snapshot()?;
    emu.netplay_restore(&before)?;
    assert_eq!(emu.netplay_snapshot()?, before);
    Ok(())
}

#[test]
fn queued_peers_confirm_input_driven_machine_state() -> Result<()> {
    let mut machines = [emulator()?, emulator()?];
    let settings = |player| Settings {
        player,
        session: [42; 16],
        input_delay: 0,
        rollback_frames: 8,
    };
    let mut peers = [
        Connection::with_transport(
            settings(0),
            PacketQueue::default(),
            &mut machines[0],
            &safe_config()?,
        )?,
        Connection::with_transport(
            settings(1),
            PacketQueue::default(),
            &mut machines[1],
            &safe_config()?,
        )?,
    ];
    let inputs = [
        Input {
            buttons: 1 | 16,
            ..Default::default()
        },
        Input {
            buttons: 2 | 32,
            ..Default::default()
        },
    ];
    for _ in 0..200 {
        for player in 0..2 {
            let advance = peers[player].status().frame < 60;
            peers[player].step(&mut machines[player], inputs[player], advance)?;
            while let Some(packet) = peers[player].transport_mut().pop() {
                peers[1 - player].transport_mut().push(&packet)?;
            }
        }
        if peers.iter().all(|p| p.status().checked_frame == 60) {
            break;
        }
    }
    for peer in &peers {
        assert_eq!(peer.status().checked_frame, 60);
    }
    // An independent uninterrupted machine proves that equal hashes did not
    // result from both transport adapters silently dropping the same input.
    let mut baseline = emulator()?;
    for _ in 0..60 {
        EmulatedMachine(&mut baseline).frame(inputs, [0; 16], false)?;
    }
    for machine in &machines {
        assert_eq!(machine.netplay_snapshot()?, baseline.netplay_snapshot()?);
        assert!(machine.bus().input.ports[0].up);
        assert!(machine.bus().input.ports[0].fire);
        assert!(machine.bus().input.ports[1].down);
        assert!(machine.bus().input.ports[1].button2);
    }
    Ok(())
}

#[test]
fn recognized_session_reports_incompatible_build_but_ignores_other_sessions() -> Result<()> {
    for offset in [4, 6] {
        let mut machine = emulator()?;
        let mut peer = Connection::with_transport(
            Settings {
                player: 0,
                session: [42; 16],
                input_delay: 0,
                rollback_frames: 8,
            },
            PacketQueue::default(),
            &mut machine,
            &safe_config()?,
        )?;
        peer.step(&mut machine, Input::default(), false)?;
        let mut packet = peer.transport_mut().pop().unwrap();
        packet[offset] ^= 1;
        packet[10] ^= 1;
        peer.transport_mut().push(&packet)?;
        peer.step(&mut machine, Input::default(), false)?;
        packet[10] ^= 1;
        peer.transport_mut().push(&packet)?;
        assert!(peer
            .step(&mut machine, Input::default(), false)
            .unwrap_err()
            .to_string()
            .contains("incompatible build"));
    }
    Ok(())
}

#[test]
fn udp_transport_discards_foreign_source_and_keeps_expected_peer() -> Result<()> {
    let expected = UdpSocket::bind("127.0.0.1:0")?;
    let foreign = UdpSocket::bind("127.0.0.1:0")?;
    let mut transport = UdpTransport::new(options(expected.local_addr()?, 0))?;
    foreign.send_to(&[1], transport.socket.local_addr()?)?;
    let mut bytes = [0; 8];
    let receive = |transport: &mut UdpTransport, bytes: &mut [u8]| -> Result<Option<usize>> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(len) = transport.receive(bytes)? {
                return Ok(Some(len));
            }
            ensure!(Instant::now() < deadline, "loopback packet did not arrive");
            std::thread::yield_now();
        }
    };
    assert_eq!(receive(&mut transport, &mut bytes)?, Some(0));
    expected.send_to(&[2, 3], transport.socket.local_addr()?)?;
    assert_eq!(receive(&mut transport, &mut bytes)?, Some(2));
    assert_eq!(&bytes[..2], &[2, 3]);
    Ok(())
}

#[test]
fn mouse_prediction_holds_buttons_without_repeating_motion() -> Result<()> {
    let mut machine = emulator()?;
    machine
        .bus_mut()
        .input
        .set_port_device(1, crate::bus::PortDevice::Mouse);
    let mut rb = Rollback::new(0, 0, 8);
    rb.receive(
        0,
        Input {
            mouse_dx: -17,
            mouse_dy: 23,
            mouse_buttons: 7,
            ..Default::default()
        },
    )?;
    for _ in 0..4 {
        assert!(rb.advance(&mut EmulatedMachine(&mut machine), Input::default())?);
        assert_eq!(machine.bus().input.joydat(1), 0x17ef);
        let port = &machine.bus().input.ports[1];
        assert!(port.fire && port.button2 && port.button3);
    }
    // A future packet must not seed motion or held buttons on an earlier frame.
    rb.receive(
        6,
        Input {
            mouse_dx: 80,
            mouse_dy: -80,
            ..Default::default()
        },
    )?;
    assert!(rb.advance(&mut EmulatedMachine(&mut machine), Input::default())?);
    assert_eq!(machine.bus().input.joydat(1), 0x17ef);
    Ok(())
}

#[test]
fn mouse_motion_is_consumed_once_when_sampling_through_stalls() -> Result<()> {
    let mut machine = emulator()?;
    machine
        .bus_mut()
        .input
        .set_port_device(0, crate::bus::PortDevice::Mouse);
    let mut cfg = crate::config::Config::try_from(crate::config::RawConfig::default())?;
    cfg.serial.mode = crate::config::SerialMode::Off;
    let settings = Settings {
        player: 0,
        session: [23; 16],
        input_delay: 0,
        rollback_frames: 1,
    };
    let mut peer =
        Connection::with_transport(settings, PacketQueue::default(), &mut machine, &cfg)?;
    let mut pending: LocalInput = Input {
        mouse_dx: 250,
        mouse_dy: -250,
        mouse_buttons: 7,
        ..Default::default()
    }
    .into();
    assert!(!peer.step_local(&mut machine, &mut pending, true)?);
    assert_eq!(pending.mouse_pending.0, 250, "handshake must retain motion");
    // Isolate sampling from the handshake already exercised by the paired tests.
    peer.connected = true;
    assert!(!peer.step_local(&mut machine, &mut pending, false)?);
    assert_eq!(
        pending.mouse_pending.0, 250,
        "confirmation polls must retain motion"
    );
    assert!(peer.step_local(&mut machine, &mut pending, true)?);
    assert_eq!(pending.mouse_pending.0, 150);
    assert!(!peer.step_local(&mut machine, &mut pending, true)?);
    assert_eq!(
        pending.mouse_pending.0, 50,
        "a stalled frame is sampled once"
    );
    pending.add_mouse_delta(7, -7);
    assert!(!peer.step_local(&mut machine, &mut pending, true)?);
    assert_eq!(
        pending.mouse_pending.0, 57,
        "new motion waits for the next unsampled frame"
    );
    peer.rollback.receive(0, Input::default())?;
    peer.rollback.acknowledge(2)?;
    assert!(peer.step_local(&mut machine, &mut pending, true)?);
    assert_eq!(pending.mouse_pending.0, 57);
    peer.rollback.receive(1, Input::default())?;
    assert!(peer.step_local(&mut machine, &mut pending, true)?);
    assert_eq!(pending.mouse_pending, (0, 0));
    assert_eq!(pending.held.mouse_buttons, 7);
    assert_eq!(
        machine.bus().input.joydat(0),
        0xff01,
        "257 counts wrap in hardware exactly once"
    );
    Ok(())
}

#[test]
fn large_pending_mouse_motion_reaches_the_wire_without_truncation() -> Result<()> {
    let mut machine = emulator()?;
    machine
        .bus_mut()
        .input
        .set_port_device(0, crate::bus::PortDevice::Mouse);
    let mut peer = Connection::with_transport(
        Settings {
            player: 0,
            session: [24; 16],
            input_delay: 0,
            rollback_frames: 1,
        },
        PacketQueue::default(),
        &mut machine,
        &safe_config()?,
    )?;
    let mut pending = LocalInput::default();
    pending.add_mouse_delta(70_000, -90_000);
    let mut transmitted = BTreeMap::new();
    for frame in 0..900 {
        let remote = wire::Packet {
            session: [24; 16],
            identity: peer.identity,
            player: 1,
            ready: true,
            delay: 0,
            window: 1,
            ack: frame,
            inputs: vec![(frame, Input::default())],
            checksum: None,
        };
        peer.transport_mut().push(&remote.encode())?;
        assert!(peer.step_local(&mut machine, &mut pending, true)?);
        while let Some(bytes) = peer.transport_mut().pop() {
            for (number, input) in wire::Packet::decode(&bytes).unwrap().inputs {
                assert!(input.mouse_dx.abs() <= 100 && input.mouse_dy.abs() <= 100);
                transmitted.insert(number, input);
            }
        }
    }
    assert_eq!(pending.mouse_pending, (0, 0));
    let total = transmitted.values().fold((0i32, 0i32), |(x, y), input| {
        (x + i32::from(input.mouse_dx), y + i32::from(input.mouse_dy))
    });
    assert_eq!(total, (70_000, -90_000));
    Ok(())
}
