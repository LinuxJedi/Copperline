// SPDX-License-Identifier: GPL-3.0-or-later

//! A lightweight, headless-friendly CPU/memory debugger driven by `COPPERLINE_DBG_*`
//! environment variables. It supports PC breakpoints, memory write-watchpoints,
//! per-hit register and memory dumps, a screenshot of the current frame on each
//! hit, and a raw instruction trace within a time window. All output goes
//! through `log` at info level. It is meant for investigating demo/timing
//! behaviour during `--screenshot-after` runs, where attaching an interactive
//! debugger is not practical.
//!
//! Configuration (all optional; the debugger stays disabled unless at least one
//! of BREAK / WATCH / TRACE is set). Addresses are hex, with or without `0x`:
//!
//! ```text
//! COPPERLINE_DBG_BREAK   = comma-separated PCs to break on     e.g. "C033C2,C033C8"
//! COPPERLINE_DBG_WATCH   = comma-separated write-watch ranges,
//!                      "ADDR" or "ADDR:LEN" (LEN in bytes) e.g. "C09580:2"
//! COPPERLINE_DBG_DUMP    = comma-separated "ADDR:WORDS" memory
//!                      regions hexdumped on every hit      e.g. "C09580:4"
//! COPPERLINE_DBG_TRACE   = set to log every executed instruction in the window
//! COPPERLINE_DBG_TRACE_FULL = like TRACE, but each line is a fixed-width all-hex
//!                      record of the whole register file (D0-D7/A0-A7) and CCR,
//!                      for diffing against a reference 68000 (e.g. vAmiga). Implies
//!                      TRACE. Format: "ft pc=.. op=.. ccr=.. d=.. a=.. | <disasm>"
//! COPPERLINE_DBG_TRACE_LO/HI = only trace instructions with LO <= pc <= HI, to
//!                      isolate one routine (e.g. a depacker loop) from the rest
//!                      of the system     e.g. LO="DE488" HI="DE578"
//! COPPERLINE_DBG_AFTER   = emulated seconds before which the debugger is inert
//! COPPERLINE_DBG_UNTIL   = emulated seconds after which the debugger is inert
//! COPPERLINE_DBG_MAXHITS = stop reporting after this many hits (default 200)
//! COPPERLINE_DBG_SHOT    = path prefix; saves "<prefix>-<seq>.png" of the current
//!                      frame on each breakpoint/watch hit
//! COPPERLINE_DBG_COPPER  = disassemble the Copper list once when the debugger
//!                      first activates. "auto"/"1" uses the live COP1LC;
//!                      "ADDR" / "ADDR:LEN" dumps LEN instructions from ADDR
//!                      e.g. "auto:64" or "C00100:200"
//! ```
//!
//! The instruction trace (COPPERLINE_DBG_TRACE) disassembles each executed
//! instruction (see `crate::disasm`).

/// A memory write-watch range, `[addr, addr+len)`.
pub struct Watch {
    pub addr: u32,
    pub len: u32,
}

/// The 68000 address-bus width the interactive debugger compares PCs and
/// watch addresses through (A0-A23).
pub const UI_ADDR_MASK: u32 = 0x00FF_FFFF;

/// An interactive memory watchpoint: a 16-bit word and the value it held
/// when the watch was set or last hit. The CPU loop stops when the live
/// word differs, whoever wrote it (CPU, Copper, blitter, disk DMA).
pub struct UiWatch {
    pub addr: u32,
    pub last: u16,
}

/// Why the interactive debugger stopped the machine.
pub enum DebugStop {
    /// The next instruction's address matches a breakpoint (it has not
    /// executed yet).
    Breakpoint { pc: u32 },
    /// A watched memory word changed during the last instruction.
    Watch {
        addr: u32,
        old: u16,
        new: u16,
        writer_pc: u32,
    },
    /// A watched custom chipset register was written (by any source: CPU
    /// or Copper), at the given beam position.
    ChipReg {
        off: u16,
        value: u16,
        source: &'static str,
        vpos: u16,
        hpos: u16,
    },
}

impl DebugStop {
    /// A one-line human-readable reason, shown as the OSD/panel message.
    pub fn describe(&self) -> String {
        match self {
            DebugStop::Breakpoint { pc } => format!("Breakpoint at ${pc:06X}"),
            DebugStop::Watch {
                addr,
                old,
                new,
                writer_pc,
            } => format!("Watch ${addr:06X}: {old:04X}->{new:04X} (pc ${writer_pc:06X})"),
            DebugStop::ChipReg {
                off,
                value,
                source,
                vpos,
                hpos,
            } => format!(
                "{} = {value:04X} ({source} write, v{vpos} h{hpos})",
                custom_reg_name(*off)
            ),
        }
    }
}

/// The hardware name of a custom-register word offset into $DFF000
/// ($000-$1FE), e.g. 0x096 -> "DMACON". Banked registers (audio channels,
/// bitplane/sprite pointers and data, colors) are derived; offsets without
/// an assigned register fall back to the hex offset.
pub fn custom_reg_name(off: u16) -> String {
    let off = off & 0x1FE;
    match off {
        0x0A0..=0x0DE => {
            let channel = (off - 0x0A0) / 0x10;
            const PARTS: [&str; 6] = ["LCH", "LCL", "LEN", "PER", "VOL", "DAT"];
            if let Some(part) = PARTS.get(((off & 0x0E) >> 1) as usize) {
                return format!("AUD{channel}{part}");
            }
        }
        0x0E0..=0x0FE => {
            let plane = (off - 0x0E0) / 4 + 1;
            let half = if off & 2 == 0 { "H" } else { "L" };
            return format!("BPL{plane}PT{half}");
        }
        0x110..=0x11E => {
            return format!("BPL{}DAT", (off - 0x110) / 2 + 1);
        }
        0x120..=0x13E => {
            let sprite = (off - 0x120) / 4;
            let half = if off & 2 == 0 { "H" } else { "L" };
            return format!("SPR{sprite}PT{half}");
        }
        0x140..=0x17E => {
            let sprite = (off - 0x140) / 8;
            const PARTS: [&str; 4] = ["POS", "CTL", "DATA", "DATB"];
            return format!("SPR{sprite}{}", PARTS[((off >> 1) & 3) as usize]);
        }
        0x180..=0x1BE => {
            return format!("COLOR{:02}", (off - 0x180) / 2);
        }
        _ => {}
    }
    let fixed = match off {
        0x000 => "BLTDDAT",
        0x002 => "DMACONR",
        0x004 => "VPOSR",
        0x006 => "VHPOSR",
        0x008 => "DSKDATR",
        0x00A => "JOY0DAT",
        0x00C => "JOY1DAT",
        0x00E => "CLXDAT",
        0x010 => "ADKCONR",
        0x012 => "POT0DAT",
        0x014 => "POT1DAT",
        0x016 => "POTGOR",
        0x018 => "SERDATR",
        0x01A => "DSKBYTR",
        0x01C => "INTENAR",
        0x01E => "INTREQR",
        0x020 => "DSKPTH",
        0x022 => "DSKPTL",
        0x024 => "DSKLEN",
        0x026 => "DSKDAT",
        0x028 => "REFPTR",
        0x02A => "VPOSW",
        0x02C => "VHPOSW",
        0x02E => "COPCON",
        0x030 => "SERDAT",
        0x032 => "SERPER",
        0x034 => "POTGO",
        0x036 => "JOYTEST",
        0x038 => "STREQU",
        0x03A => "STRVBL",
        0x03C => "STRHOR",
        0x03E => "STRLONG",
        0x040 => "BLTCON0",
        0x042 => "BLTCON1",
        0x044 => "BLTAFWM",
        0x046 => "BLTALWM",
        0x048 => "BLTCPTH",
        0x04A => "BLTCPTL",
        0x04C => "BLTBPTH",
        0x04E => "BLTBPTL",
        0x050 => "BLTAPTH",
        0x052 => "BLTAPTL",
        0x054 => "BLTDPTH",
        0x056 => "BLTDPTL",
        0x058 => "BLTSIZE",
        0x05A => "BLTCON0L",
        0x05C => "BLTSIZV",
        0x05E => "BLTSIZH",
        0x060 => "BLTCMOD",
        0x062 => "BLTBMOD",
        0x064 => "BLTAMOD",
        0x066 => "BLTDMOD",
        0x070 => "BLTCDAT",
        0x072 => "BLTBDAT",
        0x074 => "BLTADAT",
        0x078 => "SPRHDAT",
        0x07A => "BPLHDAT",
        0x07C => "DENISEID",
        0x07E => "DSKSYNC",
        0x080 => "COP1LCH",
        0x082 => "COP1LCL",
        0x084 => "COP2LCH",
        0x086 => "COP2LCL",
        0x088 => "COPJMP1",
        0x08A => "COPJMP2",
        0x08C => "COPINS",
        0x08E => "DIWSTRT",
        0x090 => "DIWSTOP",
        0x092 => "DDFSTRT",
        0x094 => "DDFSTOP",
        0x096 => "DMACON",
        0x098 => "CLXCON",
        0x09A => "INTENA",
        0x09C => "INTREQ",
        0x09E => "ADKCON",
        0x100 => "BPLCON0",
        0x102 => "BPLCON1",
        0x104 => "BPLCON2",
        0x106 => "BPLCON3",
        0x108 => "BPL1MOD",
        0x10A => "BPL2MOD",
        0x10C => "BPLCON4",
        0x10E => "CLXCON2",
        0x1C0 => "HTOTAL",
        0x1C2 => "HSSTOP",
        0x1C4 => "HBSTRT",
        0x1C6 => "HBSTOP",
        0x1C8 => "VTOTAL",
        0x1CA => "VSSTOP",
        0x1CC => "VBSTRT",
        0x1CE => "VBSTOP",
        0x1D0 => "SPRHSTRT",
        0x1D2 => "SPRHSTOP",
        0x1D4 => "BPLHSTRT",
        0x1D6 => "BPLHSTOP",
        0x1D8 => "HHPOSW",
        0x1DA => "HHPOSR",
        0x1DC => "BEAMCON0",
        0x1DE => "HSSTRT",
        0x1E0 => "VSSTRT",
        0x1E2 => "HCENTER",
        0x1E4 => "DIWHIGH",
        0x1E6 => "BPLHMOD",
        0x1E8 => "SPRHPTH",
        0x1EA => "SPRHPTL",
        0x1EC => "BPLHPTH",
        0x1EE => "BPLHPTL",
        0x1FC => "FMODE",
        0x1FE => "NO-OP",
        _ => return format!("${off:03X}"),
    };
    fixed.to_string()
}

/// The debugger window's breakpoint/watchpoint set. Owned by the CPU
/// machine so it stays armed while the window is closed; `armed` is the
/// single per-instruction gate the hot loop checks.
#[derive(Default)]
pub struct InteractiveBreaks {
    pub breakpoints: Vec<u32>,
    pub watches: Vec<UiWatch>,
    /// Watched custom-register word offsets into $DFF000 ($000-$1FE).
    /// Hits are recorded by the bus's custom-register write path (which
    /// sees every writer, CPU and Copper alike), so the offsets are
    /// mirrored into the Bus whenever this list changes.
    pub reg_watches: Vec<u16>,
    armed: bool,
}

impl InteractiveBreaks {
    pub fn armed(&self) -> bool {
        self.armed
    }

    fn rearm(&mut self) {
        self.armed = !(self.breakpoints.is_empty()
            && self.watches.is_empty()
            && self.reg_watches.is_empty());
    }

    pub fn is_breakpoint(&self, pc: u32) -> bool {
        self.breakpoints.contains(&pc)
    }

    /// Add the breakpoint, or remove it when already set. Returns true
    /// when the breakpoint is now set.
    pub fn toggle_breakpoint(&mut self, addr: u32) -> bool {
        let addr = addr & UI_ADDR_MASK;
        let added = match self.breakpoints.iter().position(|&pc| pc == addr) {
            Some(pos) => {
                self.breakpoints.remove(pos);
                false
            }
            None => {
                self.breakpoints.push(addr);
                true
            }
        };
        self.rearm();
        added
    }

    /// Add a word watch at `addr` (recording `current` as its baseline),
    /// or remove it when already set. Returns true when now set.
    pub fn toggle_watch(&mut self, addr: u32, current: u16) -> bool {
        let added = match self.watches.iter().position(|w| w.addr == addr) {
            Some(pos) => {
                self.watches.remove(pos);
                false
            }
            None => {
                self.watches.push(UiWatch {
                    addr,
                    last: current,
                });
                true
            }
        };
        self.rearm();
        added
    }

    /// Add a custom-register write watch (the offset is normalized into
    /// $000-$1FE, so both `$DFF096` and `96` address DMACON), or remove it
    /// when already set. Returns true when now set.
    pub fn toggle_reg_watch(&mut self, off: u16) -> bool {
        let off = off & 0x1FE;
        let added = match self.reg_watches.iter().position(|&o| o == off) {
            Some(pos) => {
                self.reg_watches.remove(pos);
                false
            }
            None => {
                self.reg_watches.push(off);
                true
            }
        };
        self.rearm();
        added
    }

    pub fn clear(&mut self) {
        self.breakpoints.clear();
        self.watches.clear();
        self.reg_watches.clear();
        self.armed = false;
    }
}

/// A one-shot memory dump request (`COPPERLINE_DBG_RAMDUMP=ADDR:LEN:FILE`,
/// hex ADDR/LEN), written the first time the debugger is active.
#[derive(Clone)]
pub struct RamDumpReq {
    pub addr: u32,
    pub len: u32,
    pub path: String,
}

/// A one-shot Copper-list disassembly request (`COPPERLINE_DBG_COPPER`).
#[derive(Clone)]
pub struct CopperDumpReq {
    /// List start address. `None` means "use the live COP1LC pointer".
    pub addr: Option<u32>,
    /// Maximum number of Copper instructions to disassemble.
    pub count: u32,
}

pub struct Debugger {
    pub breakpoints: Vec<u32>,
    pub watches: Vec<Watch>,
    /// `(addr, words)` regions hexdumped (as 16-bit words) on each hit.
    pub dumps: Vec<(u32, u32)>,
    pub trace: bool,
    /// COPPERLINE_DBG_TRACE_FULL: emit every CPU register (D0-D7/A0-A7) plus the
    /// CCR flags on each traced instruction, for differential comparison against
    /// a reference 68000 (vAmiga). Implies `trace`.
    pub trace_full: bool,
    /// COPPERLINE_DBG_TRACE_LO/HI: when set, only trace instructions whose PC is
    /// in `[lo, hi]`. Keeps a focused routine (e.g. a depacker loop) out of the
    /// noise of the rest of the system. `lo`=0/`hi`=u32::MAX means no filter.
    pub trace_lo: u32,
    pub trace_hi: u32,
    pub after_secs: f64,
    pub until_secs: f64,
    pub max_hits: u64,
    pub hits: u64,
    pub shot_prefix: Option<String>,
    pub shot_seq: u32,
    pub trace_lines: u64,
    /// One-shot Copper-list disassembly request, performed the first time
    /// the debugger is active.
    pub copper_dump: Option<CopperDumpReq>,
    pub copper_dumped: bool,
    /// One-shot memory-to-file dump request, performed the first time the
    /// debugger is active.
    pub ram_dump: Option<RamDumpReq>,
    pub ram_dumped: bool,
}

impl Debugger {
    /// Build a debugger from the `COPPERLINE_DBG_*` environment, or `None` when no
    /// breakpoint, watchpoint, or trace is configured.
    pub fn from_env() -> Option<Self> {
        let breakpoints = parse_addr_list("COPPERLINE_DBG_BREAK");
        let watches = parse_watch_list("COPPERLINE_DBG_WATCH");
        let trace_full = crate::envcfg::flag("COPPERLINE_DBG_TRACE_FULL");
        let trace = trace_full || crate::envcfg::flag("COPPERLINE_DBG_TRACE");
        let trace_lo = parse_hex_var("COPPERLINE_DBG_TRACE_LO").unwrap_or(0);
        let trace_hi = parse_hex_var("COPPERLINE_DBG_TRACE_HI").unwrap_or(u32::MAX);
        let copper_dump = parse_copper_dump("COPPERLINE_DBG_COPPER");
        let ram_dump = parse_ram_dump("COPPERLINE_DBG_RAMDUMP");
        if breakpoints.is_empty()
            && watches.is_empty()
            && !trace
            && copper_dump.is_none()
            && ram_dump.is_none()
        {
            return None;
        }
        let dumps = parse_watch_list("COPPERLINE_DBG_DUMP")
            .into_iter()
            .map(|w| (w.addr, w.len))
            .collect();
        let dbg = Self {
            breakpoints,
            watches,
            dumps,
            trace,
            trace_full,
            trace_lo,
            trace_hi,
            after_secs: parse_f64("COPPERLINE_DBG_AFTER").unwrap_or(0.0),
            until_secs: parse_f64("COPPERLINE_DBG_UNTIL").unwrap_or(f64::INFINITY),
            max_hits: parse_u64("COPPERLINE_DBG_MAXHITS").unwrap_or(200),
            hits: 0,
            shot_prefix: crate::envcfg::var("COPPERLINE_DBG_SHOT"),
            shot_seq: 0,
            trace_lines: 0,
            copper_dump,
            copper_dumped: false,
            ram_dump,
            ram_dumped: false,
        };
        log::info!(
            "debugger armed: breaks={:?} watches={} dumps={} trace={} window=[{},{}) max_hits={}",
            dbg.breakpoints
                .iter()
                .map(|pc| format!("{pc:#X}"))
                .collect::<Vec<_>>(),
            dbg.watches.len(),
            dbg.dumps.len(),
            dbg.trace,
            dbg.after_secs,
            dbg.until_secs,
            dbg.max_hits,
        );
        Some(dbg)
    }

    /// Whether the debugger should act at the given emulated time. False once
    /// the hit budget is exhausted, keeping long runs from flooding the log.
    pub fn enabled_at(&self, secs: f64) -> bool {
        self.hits < self.max_hits && secs >= self.after_secs && secs < self.until_secs
    }

    pub fn is_breakpoint(&self, pc: u32) -> bool {
        self.breakpoints.contains(&pc)
    }

    /// The next screenshot path, advancing the sequence counter.
    pub fn next_shot_path(&mut self) -> Option<String> {
        let prefix = self.shot_prefix.clone()?;
        let path = format!("{prefix}-{:04}.png", self.shot_seq);
        self.shot_seq += 1;
        Some(path)
    }
}

fn parse_ram_dump(var: &str) -> Option<RamDumpReq> {
    let v = crate::envcfg::var(var)?;
    let mut parts = v.splitn(3, ':');
    let addr = parse_hex(parts.next()?)?;
    let len = parse_hex(parts.next()?)?;
    let path = parts.next()?.to_string();
    Some(RamDumpReq { addr, len, path })
}

fn parse_hex(s: &str) -> Option<u32> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(s, 16).ok()
}

fn parse_addr_list(var: &str) -> Vec<u32> {
    crate::envcfg::var(var)
        .map(|v| v.split(',').filter_map(parse_hex).collect())
        .unwrap_or_default()
}

fn parse_hex_var(var: &str) -> Option<u32> {
    crate::envcfg::var(var).and_then(|v| parse_hex(v.trim()))
}

fn parse_watch_list(var: &str) -> Vec<Watch> {
    crate::envcfg::var(var)
        .map(|v| {
            v.split(',')
                .filter_map(|item| {
                    let mut parts = item.split(':');
                    let addr = parse_hex(parts.next()?)?;
                    let len = parts
                        .next()
                        .and_then(|s| s.trim().parse::<u32>().ok())
                        .unwrap_or(2)
                        .max(1);
                    Some(Watch { addr, len })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `COPPERLINE_DBG_COPPER`. Accepted forms (LEN optional, default 256):
/// `auto`/`1`/`on` (use the live COP1LC), `ADDR`, `ADDR:LEN`, `auto:LEN`.
fn parse_copper_dump(var: &str) -> Option<CopperDumpReq> {
    let raw = crate::envcfg::var(var)?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.split(':');
    let addr_s = parts.next().unwrap_or("").trim();
    let count = parts
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(256)
        .max(1);
    let addr = match addr_s.to_ascii_lowercase().as_str() {
        "" | "auto" | "1" | "on" | "yes" | "true" => None,
        _ => Some(parse_hex(addr_s)?),
    };
    Some(CopperDumpReq { addr, count })
}

fn parse_f64(var: &str) -> Option<f64> {
    crate::envcfg::var(var).and_then(|s| s.trim().parse().ok())
}

fn parse_u64(var: &str) -> Option<u64> {
    crate::envcfg::var(var).and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_breakpoints_toggle_mask_and_arm() {
        let mut breaks = InteractiveBreaks::default();
        assert!(!breaks.armed());

        // Adding masks the address to the 68000 bus width.
        assert!(breaks.toggle_breakpoint(0xFFC0_33C2));
        assert!(breaks.armed());
        assert!(breaks.is_breakpoint(0x00C0_33C2));

        // Toggling the same (masked) address removes it and disarms.
        assert!(!breaks.toggle_breakpoint(0x00C0_33C2));
        assert!(!breaks.armed());
        assert!(!breaks.is_breakpoint(0x00C0_33C2));
    }

    #[test]
    fn interactive_watches_record_baselines_and_clear() {
        let mut breaks = InteractiveBreaks::default();
        assert!(breaks.toggle_watch(0x1000, 0xABCD));
        assert_eq!(breaks.watches[0].last, 0xABCD);
        // The register watch normalizes a full $DFFxxx address to the
        // word offset.
        assert!(breaks.toggle_reg_watch(0xF096 & 0x1FE));
        assert_eq!(breaks.reg_watches, [0x096]);
        assert!(breaks.armed());

        // Toggling off, then clearing a re-added set, disarms.
        assert!(!breaks.toggle_reg_watch(0x097));
        assert!(breaks.reg_watches.is_empty());
        assert!(breaks.armed()); // memory watch still set
        breaks.toggle_breakpoint(0x100);
        breaks.clear();
        assert!(!breaks.armed());
        assert!(breaks.breakpoints.is_empty());
        assert!(breaks.watches.is_empty());
        assert!(breaks.reg_watches.is_empty());
    }

    #[test]
    fn debug_stop_describes_each_reason() {
        assert_eq!(
            DebugStop::Breakpoint { pc: 0xC033C2 }.describe(),
            "Breakpoint at $C033C2"
        );
        assert_eq!(
            DebugStop::Watch {
                addr: 0xC09580,
                old: 0x12,
                new: 0x13,
                writer_pc: 0xC03374,
            }
            .describe(),
            "Watch $C09580: 0012->0013 (pc $C03374)"
        );
        assert_eq!(
            DebugStop::ChipReg {
                off: 0x096,
                value: 0x8020,
                source: "copper",
                vpos: 44,
                hpos: 120,
            }
            .describe(),
            "DMACON = 8020 (copper write, v44 h120)"
        );
    }

    #[test]
    fn custom_reg_names_cover_fixed_and_banked_registers() {
        assert_eq!(custom_reg_name(0x096), "DMACON");
        assert_eq!(custom_reg_name(0x097), "DMACON"); // odd byte -> word
        assert_eq!(custom_reg_name(0x180), "COLOR00");
        assert_eq!(custom_reg_name(0x1BE), "COLOR31");
        assert_eq!(custom_reg_name(0x0A4), "AUD0LEN");
        assert_eq!(custom_reg_name(0x0DA), "AUD3DAT");
        assert_eq!(custom_reg_name(0x0E0), "BPL1PTH");
        assert_eq!(custom_reg_name(0x0FE), "BPL8PTL");
        assert_eq!(custom_reg_name(0x110), "BPL1DAT");
        assert_eq!(custom_reg_name(0x120), "SPR0PTH");
        assert_eq!(custom_reg_name(0x146), "SPR0DATB");
        assert_eq!(custom_reg_name(0x178), "SPR7POS");
        assert_eq!(custom_reg_name(0x1FC), "FMODE");
        // Unassigned offsets fall back to hex.
        assert_eq!(custom_reg_name(0x068), "$068");
        assert_eq!(custom_reg_name(0x0AC), "$0AC");
    }
}
