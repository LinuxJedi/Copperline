// SPDX-License-Identifier: GPL-3.0-or-later

//! Remote GDB protocol frontend for Copperline.
//!
//! This is a host debugger transport, not an emulated Amiga device. Generic
//! GDB memory packets inspect and modify CPU-visible RAM without touching
//! memory-mapped devices; Amiga custom-chip state is exposed through `monitor`
//! commands so inspection remains side-effect-free.

use crate::debugger::{custom_reg_name, UI_ADDR_MASK};
use crate::emulator::Emulator;
use crate::timetravel::ReverseOutcome;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// Instruction budget for the `monitor stepover` / `monitor finish` helpers, so
/// a call or subroutine that never returns cannot wedge the debug server.
const MONITOR_STEP_BUDGET: usize = 5_000_000;

const TARGET_XML: &str = r#"<?xml version="1.0"?>
<target>
  <architecture>m68k</architecture>
  <feature name="org.gnu.gdb.m68k.core">
    <reg name="d0" bitsize="32" regnum="0"/>
    <reg name="d1" bitsize="32" regnum="1"/>
    <reg name="d2" bitsize="32" regnum="2"/>
    <reg name="d3" bitsize="32" regnum="3"/>
    <reg name="d4" bitsize="32" regnum="4"/>
    <reg name="d5" bitsize="32" regnum="5"/>
    <reg name="d6" bitsize="32" regnum="6"/>
    <reg name="d7" bitsize="32" regnum="7"/>
    <reg name="a0" bitsize="32" regnum="8"/>
    <reg name="a1" bitsize="32" regnum="9"/>
    <reg name="a2" bitsize="32" regnum="10"/>
    <reg name="a3" bitsize="32" regnum="11"/>
    <reg name="a4" bitsize="32" regnum="12"/>
    <reg name="a5" bitsize="32" regnum="13"/>
    <reg name="a6" bitsize="32" regnum="14"/>
    <reg name="sp" bitsize="32" regnum="15" type="data_ptr"/>
    <reg name="ps" bitsize="32" regnum="16"/>
    <reg name="pc" bitsize="32" regnum="17" type="code_ptr"/>
  </feature>
</target>
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: String,
    pub reverse_budget_mb: usize,
    pub reverse_interval_frames: u64,
}

impl Config {
    pub fn new(listen: String) -> Self {
        Self {
            listen,
            reverse_budget_mb: crate::envcfg::var("COPPERLINE_DBG_RR_BUDGET_MB")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(crate::debugger::RR_DEFAULT_BUDGET_MB),
            reverse_interval_frames: crate::envcfg::var("COPPERLINE_DBG_RR_INTERVAL")
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(crate::debugger::RR_DEFAULT_INTERVAL_FRAMES),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Watchpoint {
    addr: u32,
    len: usize,
    last: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StopReason {
    Attached,
    Step,
    Breakpoint,
    Watchpoint(u32),
    RegisterWatch,
    Reverse,
    Interrupted,
}

pub fn run(mut emu: Emulator, config: Config) -> Result<()> {
    let bind = normalize_listen_addr(&config.listen)?;
    let listener = TcpListener::bind(&bind).with_context(|| format!("binding GDB stub {bind}"))?;
    log::info!("gdb: listening on {bind}");
    let (stream, peer) = listener.accept().context("accepting GDB connection")?;
    log::info!("gdb: connection from {peer}");
    stream.set_nodelay(true).ok();

    emu.set_paced(false);
    emu.enable_time_travel(config.reverse_budget_mb, config.reverse_interval_frames);
    emu.debug_ensure_time_travel_anchor()?;

    Session::new(emu, stream).run()
}

fn normalize_listen_addr(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("--gdb requires ADDR, :PORT, or PORT"));
    }
    if trimmed.starts_with(':') {
        return Ok(format!("127.0.0.1{trimmed}"));
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Ok(format!("127.0.0.1:{trimmed}"));
    }
    Ok(trimmed.to_string())
}

struct Session {
    emu: Emulator,
    stream: TcpStream,
    no_ack: bool,
    breakpoints: Vec<u32>,
    watchpoints: Vec<Watchpoint>,
    reg_watches: Vec<u16>,
    stop: StopReason,
    cpu_idle: bool,
}

impl Session {
    fn new(emu: Emulator, stream: TcpStream) -> Self {
        Self {
            emu,
            stream,
            no_ack: false,
            breakpoints: Vec::new(),
            watchpoints: Vec::new(),
            reg_watches: Vec::new(),
            stop: StopReason::Attached,
            cpu_idle: false,
        }
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let Some(packet) = self.read_packet()? else {
                return Ok(());
            };
            if packet == "QStartNoAckMode" {
                self.send_packet("OK")?;
                self.no_ack = true;
                continue;
            }
            match self.handle_packet(&packet)? {
                PacketOutcome::Reply(reply) => self.send_packet(&reply)?,
                PacketOutcome::Disconnect => {
                    self.send_packet("OK")?;
                    return Ok(());
                }
            }
        }
    }

    fn handle_packet(&mut self, packet: &str) -> Result<PacketOutcome> {
        let reply = match packet {
            "!" => "OK".to_string(),
            "?" => self.stop_reply(),
            "g" => self.read_all_registers(),
            "qC" => "QC1".to_string(),
            "qAttached" | "qAttached:1" => "1".to_string(),
            "qfThreadInfo" => "m1".to_string(),
            "qsThreadInfo" => "l".to_string(),
            "vCont?" => "vCont;c;s".to_string(),
            "D" | "D;1" => return Ok(PacketOutcome::Disconnect),
            "k" => return Ok(PacketOutcome::Disconnect),
            _ if packet.starts_with("qSupported") => {
                "PacketSize=4000;QStartNoAckMode+;qXfer:features:read+;hwbreak+;ReverseStep+;ReverseContinue+".to_string()
            }
            _ if packet.starts_with("qXfer:features:read:target.xml:") => {
                self.read_target_xml(packet)?
            }
            _ if packet.starts_with("qRcmd,") => {
                let command = String::from_utf8(hex_decode(&packet[6..])?)
                    .context("decoding monitor command")?;
                let output = self.handle_monitor(command.trim())?;
                self.send_console(&output)?;
                "OK".to_string()
            }
            _ if packet.starts_with('H') => "OK".to_string(),
            _ if packet.starts_with('T') => "OK".to_string(),
            _ if packet.starts_with('p') => self.read_register(&packet[1..])?,
            _ if packet.starts_with('P') => self.write_register(&packet[1..])?,
            _ if packet.starts_with('m') => self.read_memory(&packet[1..])?,
            _ if packet.starts_with('M') => self.write_memory(&packet[1..])?,
            _ if packet.starts_with("Z0,") || packet.starts_with("Z1,") => {
                self.add_breakpoint(packet)?
            }
            _ if packet.starts_with("z0,") || packet.starts_with("z1,") => {
                self.remove_breakpoint(packet)?
            }
            _ if packet.starts_with("Z2,") || packet.starts_with("Z3,") || packet.starts_with("Z4,") => {
                self.add_watchpoint(packet)?
            }
            _ if packet.starts_with("z2,") || packet.starts_with("z3,") || packet.starts_with("z4,") => {
                self.remove_watchpoint(packet)?
            }
            _ if packet.starts_with('s') => {
                if let Some(addr) = packet.strip_prefix('s').filter(|s| !s.is_empty()) {
                    let pc = parse_hex_u32(addr)?;
                    self.emu.machine.debug_set_register(17, pc);
                }
                self.step_forward()?
            }
            _ if packet.starts_with('c') => {
                if let Some(addr) = packet.strip_prefix('c').filter(|s| !s.is_empty()) {
                    let pc = parse_hex_u32(addr)?;
                    self.emu.machine.debug_set_register(17, pc);
                }
                self.continue_forward()?
            }
            _ if packet.starts_with("vCont;c") => self.continue_forward()?,
            _ if packet.starts_with("vCont;s") => self.step_forward()?,
            "bs" => self.reverse_step()?,
            "bc" => self.reverse_continue()?,
            _ => String::new(),
        };
        Ok(PacketOutcome::Reply(reply))
    }

    /// Hard cap on a single packet's payload, well above the `PacketSize=4000`
    /// this stub advertises in qSupported but far short of unbounded: this is
    /// network-facing input (the stub can bind non-loopback), so a peer that
    /// never sends the `#` terminator must not be able to grow `payload`
    /// without limit.
    const MAX_PACKET_PAYLOAD_BYTES: usize = 1 << 20;

    fn read_packet(&mut self) -> Result<Option<String>> {
        // Loop (rather than recurse) on a checksum mismatch: a peer sending
        // many consecutive bad-checksum packets must not grow the call
        // stack without bound.
        loop {
            let mut byte = [0u8; 1];
            loop {
                match self.stream.read_exact(&mut byte) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                    Err(e) => return Err(e).context("reading GDB packet"),
                }
                match byte[0] {
                    b'+' | b'-' => continue,
                    b'$' => break,
                    0x03 => {
                        self.stop = StopReason::Interrupted;
                        return Ok(Some("?".to_string()));
                    }
                    _ => continue,
                }
            }

            let mut payload = Vec::new();
            loop {
                self.stream
                    .read_exact(&mut byte)
                    .context("reading GDB packet payload")?;
                if byte[0] == b'#' {
                    break;
                }
                payload.push(byte[0]);
                if payload.len() > Self::MAX_PACKET_PAYLOAD_BYTES {
                    bail!(
                        "GDB packet exceeds {}-byte limit without a '#' terminator",
                        Self::MAX_PACKET_PAYLOAD_BYTES
                    );
                }
            }
            let mut sum_bytes = [0u8; 2];
            self.stream
                .read_exact(&mut sum_bytes)
                .context("reading GDB packet checksum")?;
            let expected = parse_hex_byte(sum_bytes[0], sum_bytes[1])?;
            let actual = checksum(&payload);
            if expected != actual {
                if !self.no_ack {
                    self.stream.write_all(b"-").ok();
                }
                continue;
            }
            if !self.no_ack {
                self.stream.write_all(b"+").ok();
            }
            return String::from_utf8(payload)
                .map(Some)
                .context("GDB packet is not UTF-8");
        }
    }

    fn send_packet(&mut self, payload: &str) -> Result<()> {
        let sum = checksum(payload.as_bytes());
        write!(self.stream, "${payload}#{sum:02x}").context("sending GDB packet")?;
        self.stream.flush().ok();
        Ok(())
    }

    fn send_console(&mut self, output: &str) -> Result<()> {
        for line in output.as_bytes().chunks(200) {
            self.send_packet(&format!("O{}", hex_encode(line)))?;
        }
        Ok(())
    }

    fn stop_reply(&self) -> String {
        match &self.stop {
            StopReason::Watchpoint(addr) => format!("T05watch:{addr:x};thread:1;"),
            StopReason::RegisterWatch => "T05thread:1;".to_string(),
            StopReason::Breakpoint => "T05hwbreak:;thread:1;".to_string(),
            _ => "T05thread:1;".to_string(),
        }
    }

    fn read_all_registers(&self) -> String {
        let mut out = String::with_capacity(18 * 8);
        for reg in 0..18 {
            let value = self.emu.machine.debug_register(reg).unwrap_or(0);
            out.push_str(&format!("{value:08x}"));
        }
        out
    }

    fn read_register(&self, reg: &str) -> Result<String> {
        let reg = parse_hex_usize(reg)?;
        Ok(match self.emu.machine.debug_register(reg) {
            Some(value) => format!("{value:08x}"),
            None => "E00".to_string(),
        })
    }

    fn write_register(&mut self, payload: &str) -> Result<String> {
        let Some((reg_s, value_s)) = payload.split_once('=') else {
            return Ok("E01".to_string());
        };
        let reg = parse_hex_usize(reg_s)?;
        let bytes = hex_decode(value_s)?;
        let mut value = 0u32;
        for byte in bytes.iter().take(4) {
            value = (value << 8) | u32::from(*byte);
        }
        Ok(if self.emu.machine.debug_set_register(reg, value) {
            self.emu.machine.refresh_irq_line();
            "OK".to_string()
        } else {
            "E00".to_string()
        })
    }

    fn read_memory(&self, payload: &str) -> Result<String> {
        let Some((addr_s, len_s)) = payload.split_once(',') else {
            return Ok("E01".to_string());
        };
        let addr = parse_hex_u32(addr_s)?;
        let len = parse_hex_usize(len_s)?;
        // `len` is a hex value the peer controls directly (independent of
        // the packet's own byte length), so a tiny packet like "m0,ffffffff"
        // could otherwise demand a multi-GB allocation. No real GDB request
        // needs more than a small fraction of the address space at once.
        if len > Self::MAX_PACKET_PAYLOAD_BYTES {
            return Ok("E01".to_string());
        }
        Ok(hex_encode(&self.emu.machine.debug_read_memory(addr, len)))
    }

    fn write_memory(&mut self, payload: &str) -> Result<String> {
        let Some((range, data_s)) = payload.split_once(':') else {
            return Ok("E01".to_string());
        };
        let Some((addr_s, len_s)) = range.split_once(',') else {
            return Ok("E01".to_string());
        };
        let addr = parse_hex_u32(addr_s)?;
        let len = parse_hex_usize(len_s)?;
        let data = hex_decode(data_s)?;
        if data.len() != len {
            return Ok("E02".to_string());
        }
        let written = self.emu.machine.debug_write_memory(addr, &data);
        self.refresh_watchpoints();
        Ok(if written == len {
            "OK".to_string()
        } else {
            "E03".to_string()
        })
    }

    fn add_breakpoint(&mut self, packet: &str) -> Result<String> {
        let (addr, _) = parse_z_packet(packet)?;
        let addr = addr & UI_ADDR_MASK;
        if !self.breakpoints.contains(&addr) {
            self.breakpoints.push(addr);
        }
        Ok("OK".to_string())
    }

    fn remove_breakpoint(&mut self, packet: &str) -> Result<String> {
        let (addr, _) = parse_z_packet(packet)?;
        let addr = addr & UI_ADDR_MASK;
        self.breakpoints.retain(|&candidate| candidate != addr);
        Ok("OK".to_string())
    }

    fn add_watchpoint(&mut self, packet: &str) -> Result<String> {
        let (addr, len) = parse_z_packet(packet)?;
        let len = len.max(1);
        let last = self.emu.machine.debug_read_memory(addr, len);
        if let Some(existing) = self
            .watchpoints
            .iter_mut()
            .find(|w| w.addr == addr && w.len == len)
        {
            existing.last = last;
        } else {
            self.watchpoints.push(Watchpoint { addr, len, last });
        }
        Ok("OK".to_string())
    }

    fn remove_watchpoint(&mut self, packet: &str) -> Result<String> {
        let (addr, len) = parse_z_packet(packet)?;
        self.watchpoints
            .retain(|watch| watch.addr != addr || watch.len != len.max(1));
        Ok("OK".to_string())
    }

    fn step_forward(&mut self) -> Result<String> {
        self.stop = StopReason::Step;
        self.emu.debug_step_for_gdb(&mut self.cpu_idle)?;
        if let Some(stop) = self.check_stop()? {
            self.stop = stop;
        }
        Ok(self.stop_reply())
    }

    fn continue_forward(&mut self) -> Result<String> {
        loop {
            self.emu.debug_step_for_gdb(&mut self.cpu_idle)?;
            if let Some(stop) = self.check_stop()? {
                self.stop = stop;
                return Ok(self.stop_reply());
            }
            if self.poll_interrupt()? {
                self.stop = StopReason::Interrupted;
                return Ok(self.stop_reply());
            }
        }
    }

    fn reverse_step(&mut self) -> Result<String> {
        match self.emu.tt_reverse_step(1)? {
            ReverseOutcome::Found(_) => {
                self.cpu_idle = false;
                self.refresh_watchpoints();
                self.stop = StopReason::Reverse;
                Ok(self.stop_reply())
            }
            ReverseOutcome::NotFound | ReverseOutcome::BeyondHistory => Ok("E01".to_string()),
        }
    }

    fn reverse_continue(&mut self) -> Result<String> {
        match self.emu.tt_reverse_continue_to(&self.breakpoints)? {
            ReverseOutcome::Found(_) => {
                self.cpu_idle = false;
                self.refresh_watchpoints();
                self.stop = StopReason::Breakpoint;
                Ok(self.stop_reply())
            }
            ReverseOutcome::NotFound | ReverseOutcome::BeyondHistory => Ok("E01".to_string()),
        }
    }

    fn check_stop(&mut self) -> Result<Option<StopReason>> {
        if self.emu.bus_mut().take_ui_reg_hit().is_some() {
            return Ok(Some(StopReason::RegisterWatch));
        }
        let pc = self.emu.machine.pc() & UI_ADDR_MASK;
        if self.breakpoints.contains(&pc) {
            return Ok(Some(StopReason::Breakpoint));
        }
        for watch in &mut self.watchpoints {
            let cur = self.emu.machine.debug_read_memory(watch.addr, watch.len);
            if cur != watch.last {
                watch.last = cur;
                return Ok(Some(StopReason::Watchpoint(watch.addr)));
            }
        }
        Ok(None)
    }

    fn refresh_watchpoints(&mut self) {
        for watch in &mut self.watchpoints {
            watch.last = self.emu.machine.debug_read_memory(watch.addr, watch.len);
        }
    }

    fn poll_interrupt(&mut self) -> Result<bool> {
        self.stream
            .set_nonblocking(true)
            .context("setting GDB stream nonblocking")?;
        let mut byte = [0u8; 1];
        let result = match self.stream.peek(&mut byte) {
            Ok(1) if byte[0] == 0x03 => {
                let _ = self.stream.read(&mut byte);
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e).context("polling GDB interrupt"),
        };
        self.stream
            .set_nonblocking(false)
            .context("restoring GDB stream blocking mode")?;
        result
    }

    fn read_target_xml(&self, packet: &str) -> Result<String> {
        let Some((_, range)) = packet.rsplit_once(':') else {
            return Ok("E01".to_string());
        };
        let Some((offset_s, len_s)) = range.split_once(',') else {
            return Ok("E01".to_string());
        };
        let offset = parse_hex_usize(offset_s)?;
        let len = parse_hex_usize(len_s)?;
        let bytes = TARGET_XML.as_bytes();
        if offset >= bytes.len() {
            return Ok("l".to_string());
        }
        let end = offset.saturating_add(len).min(bytes.len());
        let prefix = if end == bytes.len() { 'l' } else { 'm' };
        // TARGET_XML is pure ASCII today, so any offset/len is a valid char
        // boundary, but both come straight from the peer: don't let a future
        // non-ASCII addition (or a boundary that happens to split a
        // multi-byte char) turn into a panic instead of a clean error.
        let chunk = std::str::from_utf8(&bytes[offset..end])
            .map_err(|_| anyhow!("target XML slice landed on a non-UTF-8 boundary"))?;
        Ok(format!("{prefix}{chunk}"))
    }

    fn handle_monitor(&mut self, command: &str) -> Result<String> {
        let mut parts = command.split_whitespace();
        let Some(cmd) = parts.next() else {
            return Ok(monitor_help());
        };
        match cmd {
            "help" => Ok(monitor_help()),
            "status" => Ok(self.monitor_status()),
            "beam" => Ok(format!(
                "beam vpos={} hpos={} frame={} cck={} pos={}\n",
                self.emu.bus().agnus.vpos,
                self.emu.bus().agnus.hpos,
                self.emu.bus().emulated_frames(),
                self.emu.bus().emulated_cck(),
                self.emu.retired_instructions()
            )),
            "custom" => Ok(self.monitor_custom()),
            "stepover" => {
                // Step over a BSR/JSR/TRAP call (single step otherwise),
                // bounded so a call that never returns cannot hang the server.
                self.cpu_idle = false;
                self.emu.debug_step_over(MONITOR_STEP_BUDGET)?;
                self.refresh_watchpoints();
                Ok(format!("pc=${:08X}\n", self.emu.machine.pc()))
            }
            "finish" => {
                // Run until the current subroutine returns to its caller.
                self.cpu_idle = false;
                self.emu.debug_step_out(MONITOR_STEP_BUDGET)?;
                self.refresh_watchpoints();
                Ok(format!("pc=${:08X}\n", self.emu.machine.pc()))
            }
            "reg" => {
                let Some(name) = parts.next() else {
                    return Ok("usage: monitor reg NAME|OFFSET\n".to_string());
                };
                let Some(off) = parse_custom_reg(name) else {
                    return Ok(format!("unknown custom register {name}\n"));
                };
                Ok(self.monitor_reg(off))
            }
            "write-reg" => {
                let Some(name) = parts.next() else {
                    return Ok("usage: monitor write-reg NAME|OFFSET VALUE\n".to_string());
                };
                let Some(value_s) = parts.next() else {
                    return Ok("usage: monitor write-reg NAME|OFFSET VALUE\n".to_string());
                };
                let Some(off) = parse_custom_reg(name) else {
                    return Ok(format!("unknown custom register {name}\n"));
                };
                let value = parse_hex_u16(value_s)?;
                let irq = self
                    .emu
                    .bus_mut()
                    .custom_write(u64::from(off), 2, u64::from(value));
                if irq {
                    self.emu.machine.refresh_irq_line();
                }
                Ok(format!(
                    "{} ${off:03X} <- ${value:04X}\n",
                    custom_reg_name(off)
                ))
            }
            "watch-reg" => {
                let Some(name) = parts.next() else {
                    return Ok("usage: monitor watch-reg NAME|OFFSET\n".to_string());
                };
                let Some(off) = parse_custom_reg(name) else {
                    return Ok(format!("unknown custom register {name}\n"));
                };
                if !self.reg_watches.contains(&off) {
                    self.reg_watches.push(off);
                    self.emu.bus_mut().set_ui_reg_watches(&self.reg_watches);
                }
                Ok(format!("watching {} ${off:03X}\n", custom_reg_name(off)))
            }
            "unwatch-reg" => {
                let Some(name) = parts.next() else {
                    return Ok("usage: monitor unwatch-reg NAME|OFFSET\n".to_string());
                };
                let Some(off) = parse_custom_reg(name) else {
                    return Ok(format!("unknown custom register {name}\n"));
                };
                self.reg_watches.retain(|&candidate| candidate != off);
                self.emu.bus_mut().set_ui_reg_watches(&self.reg_watches);
                Ok(format!(
                    "not watching {} ${off:03X}\n",
                    custom_reg_name(off)
                ))
            }
            "clear-reg-watches" => {
                self.reg_watches.clear();
                self.emu.bus_mut().set_ui_reg_watches(&[]);
                Ok("cleared custom-register watches\n".to_string())
            }
            "copper" => self.monitor_copper(parts.collect()),
            "last-writer" => {
                let Some(addr_s) = parts.next() else {
                    return Ok("usage: monitor last-writer ADDR\n".to_string());
                };
                let addr = parse_hex_u32(addr_s)? & !1;
                let before = self.emu.retired_instructions();
                match self.emu.tt_last_writer(addr, before)? {
                    ReverseOutcome::Found(rec) => {
                        self.cpu_idle = false;
                        self.refresh_watchpoints();
                        Ok(format!(
                            "last writer ${:06X}: {:04X}->{:04X} pc=${:08X} pos={} frame={} cck={}\n",
                            rec.addr, rec.old, rec.new, rec.pc, rec.pos, rec.frame, rec.cck
                        ))
                    }
                    ReverseOutcome::NotFound => Ok(format!(
                        "no write to ${addr:06X} found in retained history\n"
                    )),
                    ReverseOutcome::BeyondHistory => Ok(format!(
                        "last write to ${addr:06X} predates retained history\n"
                    )),
                }
            }
            _ => Ok(format!("unknown monitor command {cmd}\n{}", monitor_help())),
        }
    }

    fn monitor_status(&self) -> String {
        format!(
            "pc=${:08X} sr=${:04X} frame={} beam=({}, {}) pos={} reverse={}\n",
            self.emu.machine.pc(),
            self.emu.machine.sr(),
            self.emu.bus().emulated_frames(),
            self.emu.bus().agnus.vpos,
            self.emu.bus().agnus.hpos,
            self.emu.retired_instructions(),
            if self.emu.time_travel_enabled() {
                "armed"
            } else {
                "off"
            }
        )
    }

    fn monitor_custom(&self) -> String {
        let bus = self.emu.bus();
        let mut out = String::new();
        out.push_str(&format!(
            "beam vpos={} hpos={} frame={}\n",
            bus.agnus.vpos,
            bus.agnus.hpos,
            bus.emulated_frames()
        ));
        out.push_str(&format!("{}\n", bus.debug_display_state()));
        for off in [
            0x002, 0x004, 0x006, 0x010, 0x01C, 0x01E, 0x080, 0x082, 0x084, 0x086, 0x096, 0x09A,
            0x09C, 0x09E, 0x100, 0x102, 0x104, 0x106, 0x108, 0x10A, 0x1FC,
        ] {
            if let Some(value) = bus.debug_custom_word(off) {
                out.push_str(&format!(
                    "{:<8} ${off:03X} = ${value:04X}\n",
                    custom_reg_name(off)
                ));
            }
        }
        out
    }

    fn monitor_reg(&self, off: u16) -> String {
        match self.emu.bus().debug_custom_word(off) {
            Some(value) => format!("{} ${off:03X} = ${value:04X}\n", custom_reg_name(off)),
            None => format!("{} ${off:03X}: no debug latch\n", custom_reg_name(off)),
        }
    }

    fn monitor_copper(&self, args: Vec<&str>) -> Result<String> {
        let bus = self.emu.bus();
        let start = match args.first().copied() {
            None | Some("auto") => bus.agnus.cop1lc,
            Some("pc") => bus.copper.pc(),
            Some(addr) => parse_hex_u32(addr)?,
        };
        let count = match args.get(1).copied() {
            Some(count) => parse_hex_usize(count)?,
            None => 64,
        };
        let mut out = format!(
            "COP1LC ${:06X} COP2LC ${:06X} COPPC ${:06X} ({})\n",
            bus.agnus.cop1lc,
            bus.agnus.cop2lc,
            bus.copper.pc(),
            if bus.copper.is_running() {
                "running"
            } else {
                "stopped"
            }
        );
        for (addr, text) in
            crate::disasm::dump_copper_list(|addr| self.emu.bus().peek_word_any(addr), start, count)
        {
            out.push_str(&format!("{addr:06X}  {text}\n"));
        }
        Ok(out)
    }
}

enum PacketOutcome {
    Reply(String),
    Disconnect,
}

fn monitor_help() -> String {
    "monitor commands:\n\
     help\n\
     status | beam | custom\n\
     stepover | finish\n\
     reg NAME|OFFSET\n\
     write-reg NAME|OFFSET VALUE\n\
     watch-reg NAME|OFFSET | unwatch-reg NAME|OFFSET | clear-reg-watches\n\
     copper [auto|pc|ADDR] [COUNT]\n\
     last-writer ADDR\n"
        .to_string()
}

fn parse_z_packet(packet: &str) -> Result<(u32, usize)> {
    let mut fields = packet.split(',');
    let _kind = fields.next();
    let addr = fields
        .next()
        .ok_or_else(|| anyhow!("missing Z/z address"))?;
    let kind = fields
        .next()
        .ok_or_else(|| anyhow!("missing Z/z kind"))?
        .split(';')
        .next()
        .unwrap_or("1");
    Ok((parse_hex_u32(addr)?, parse_hex_usize(kind)?))
}

fn parse_custom_reg(input: &str) -> Option<u16> {
    if let Ok(value) = parse_hex_u32(input) {
        return Some(custom_offset_from_value(value));
    }
    let needle = input.trim().to_ascii_uppercase();
    (0..=0x1FEu16)
        .step_by(2)
        .find(|&off| custom_reg_name(off).to_ascii_uppercase() == needle)
}

fn custom_offset_from_value(value: u32) -> u16 {
    if (0x00DF_F000..=0x00DF_FFFF).contains(&value) {
        (value - 0x00DF_F000) as u16 & 0x1FE
    } else {
        value as u16 & 0x1FE
    }
}

fn parse_hex_u16(input: &str) -> Result<u16> {
    let value = parse_hex_u32(input)?;
    Ok(value as u16)
}

fn parse_hex_u32(input: &str) -> Result<u32> {
    let trimmed = input
        .trim()
        .trim_start_matches('$')
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    u32::from_str_radix(trimmed, 16).with_context(|| format!("invalid hex value {input:?}"))
}

fn parse_hex_usize(input: &str) -> Result<usize> {
    let value = parse_hex_u32(input)?;
    Ok(value as usize)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode(input: &str) -> Result<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(anyhow!("hex string has odd length"));
    }
    bytes
        .chunks(2)
        .map(|pair| parse_hex_byte(pair[0], pair[1]))
        .collect()
}

fn parse_hex_byte(hi: u8, lo: u8) -> Result<u8> {
    let hi = hex_nibble(hi)?;
    let lo = hex_nibble(lo)?;
    Ok((hi << 4) | lo)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(anyhow!("invalid hex digit {:?}", byte as char)),
    }
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addr_defaults_to_loopback_for_port_forms() -> Result<()> {
        assert_eq!(normalize_listen_addr(":2345")?, "127.0.0.1:2345");
        assert_eq!(normalize_listen_addr("2345")?, "127.0.0.1:2345");
        assert_eq!(normalize_listen_addr("0.0.0.0:2345")?, "0.0.0.0:2345");
        Ok(())
    }

    #[test]
    fn hex_round_trip_and_checksum_match_rsp_framing() -> Result<()> {
        let data = b"m1000,10";
        assert_eq!(hex_decode(&hex_encode(data))?, data);
        assert_eq!(checksum(data), 0xbb);
        Ok(())
    }

    #[test]
    fn custom_register_parser_accepts_names_offsets_and_addresses() {
        assert_eq!(parse_custom_reg("DMACON"), Some(0x096));
        assert_eq!(parse_custom_reg("dff096"), Some(0x096));
        assert_eq!(parse_custom_reg("$96"), Some(0x096));
        assert_eq!(parse_custom_reg("COLOR00"), Some(0x180));
        assert_eq!(parse_custom_reg("notareg"), None);
    }

    #[test]
    fn target_xml_chunk_uses_rsp_more_and_last_prefixes() -> Result<()> {
        let mut bytes = TARGET_XML.as_bytes();
        let first = &bytes[..16];
        assert_eq!(first[0], b'<');
        bytes = &bytes[bytes.len() - 8..];
        assert!(!bytes.is_empty());
        Ok(())
    }
}
