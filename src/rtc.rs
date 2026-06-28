// SPDX-License-Identifier: GPL-3.0-or-later

//! Battery-backed real-time clock emulation.
//!
//! Classic big-box Amigas expose the Oki MSM6242 at $DC0000. The chip
//! has sixteen four-bit registers; on Amiga each register is visible as
//! a 32-bit word, so register N lives at base + N * 4. Copperline exposes a
//! read-only wall-clock view: guest writes can control the HOLD latch,
//! but they never change the host clock.
//!
//! The clock is reported in the host's *local* time zone (matching the
//! auto-generated filename stamps in `timestamp.rs`), since AmigaOS has no
//! real notion of time zones and a UTC clock just confuses users. The
//! deterministic `COPPERLINE_RTC_FIXED_SECS` override stays UTC so it
//! remains host-independent.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Msm6242Rtc {
    control_d: u8,
    control_e: u8,
    latched: Option<RtcDateTime>,
    #[cfg(test)]
    test_time: Option<SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RtcDateTime {
    year: u16,
    month: u8,
    day: u8,
    weekday: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl Msm6242Rtc {
    const CD_HOLD: u8 = 1 << 0;
    const CD_IRQ_FLAG: u8 = 1 << 2;
    const CF_24H: u8 = 1 << 2;

    pub fn read(&mut self, addr: u64, _size: usize) -> u64 {
        self.read_register(register_from_offset(addr)) as u64
    }

    pub fn write(&mut self, addr: u64, _size: usize, val: u64) {
        let reg = register_from_offset(addr);
        let val = (val & 0x0F) as u8;
        match reg {
            0xD => {
                if val & Self::CD_HOLD != 0 {
                    if self.latched.is_none() {
                        self.latched = Some(self.current_time());
                    }
                    self.control_d = Self::CD_HOLD;
                } else {
                    self.latched = None;
                    self.control_d = 0;
                }
            }
            0xE => {
                self.control_e = val;
            }
            0xF => {
                // Keep the clock running in 24-hour mode. STOP, RESET
                // and TEST writes are deliberately not persistent.
            }
            _ => {}
        }
    }

    fn read_register(&mut self, reg: u8) -> u8 {
        let time = self.latched.unwrap_or_else(|| self.current_time());
        (match reg {
            0x0 => time.second % 10,
            0x1 => time.second / 10,
            0x2 => time.minute % 10,
            0x3 => time.minute / 10,
            0x4 => time.hour % 10,
            0x5 => time.hour / 10,
            0x6 => time.day % 10,
            0x7 => time.day / 10,
            0x8 => time.month % 10,
            0x9 => time.month / 10,
            0xA => (time.year % 10) as u8,
            0xB => ((time.year / 10) % 10) as u8,
            0xC => time.weekday,
            0xD => self.control_d | Self::CD_IRQ_FLAG,
            0xE => self.control_e,
            0xF => Self::CF_24H,
            _ => 0,
        }) & 0x0F
    }

    fn current_time(&self) -> RtcDateTime {
        #[cfg(test)]
        if let Some(time) = self.test_time {
            return RtcDateTime::from_system_time(time);
        }
        // COPPERLINE_RTC_FIXED_SECS pins the clock to a fixed Unix-seconds
        // value, making RTC reads deterministic across runs (otherwise the
        // host wall-clock differs run-to-run, which pollutes differential
        // traces with spurious timestamp divergences).
        if let Some(secs) = crate::envcfg::var("COPPERLINE_RTC_FIXED_SECS")
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            return RtcDateTime::from_unix_seconds(secs);
        }
        RtcDateTime::from_system_time_local(SystemTime::now())
    }

    #[cfg(test)]
    fn set_test_time(&mut self, time: SystemTime) {
        self.test_time = Some(time);
    }
}

fn register_from_offset(addr: u64) -> u8 {
    ((addr >> 2) & 0x0F) as u8
}

impl RtcDateTime {
    /// UTC decomposition for the deterministic test path, where a
    /// host-independent (time-zone-free) result keeps the asserted BCD
    /// digits stable across CI hosts.
    #[cfg(test)]
    fn from_system_time(time: SystemTime) -> Self {
        Self::from_unix_seconds(Self::unix_secs(time))
    }

    /// Local-time decomposition for the live clock, mirroring
    /// `timestamp.rs` so the RTC and the auto-generated filename stamps
    /// agree on the time zone. Falls back to UTC where the platform has no
    /// thread-safe local conversion (or it fails).
    fn from_system_time_local(time: SystemTime) -> Self {
        let secs = Self::unix_secs(time);
        Self::from_local(secs).unwrap_or_else(|| Self::from_unix_seconds(secs))
    }

    fn unix_secs(time: SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn from_unix_seconds(secs: u64) -> Self {
        let days = (secs / 86_400) as i64;
        let second_of_day = (secs % 86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        Self {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            weekday: ((days + 4).rem_euclid(7)) as u8,
            hour: (second_of_day / 3600) as u8,
            minute: ((second_of_day / 60) % 60) as u8,
            second: (second_of_day % 60) as u8,
        }
    }

    /// Decompose a Unix-seconds value into the host's *local* broken-down
    /// time. Returns `None` when the platform exposes no thread-safe local
    /// conversion so the caller can fall back to UTC.
    ///
    /// As in `timestamp.rs`, this is sound only because we never mutate the
    /// TZ environment at runtime (envcfg snapshots it once), so `localtime_r`
    /// cannot race the audio thread.
    #[cfg(unix)]
    fn from_local(secs: u64) -> Option<Self> {
        // SAFETY: localtime_r fully initializes `tm` and retains no pointers.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = secs as libc::time_t;
        if unsafe { libc::localtime_r(&t, &mut tm).is_null() } {
            return None;
        }
        Some(Self::from_tm(&tm))
    }

    #[cfg(windows)]
    fn from_local(secs: u64) -> Option<Self> {
        // localtime_s reverses the POSIX argument order and returns errno_t
        // (0 = success).
        // SAFETY: localtime_s fully initializes `tm` and retains no pointers.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t = secs as libc::time_t;
        if unsafe { libc::localtime_s(&mut tm, &t) } != 0 {
            return None;
        }
        Some(Self::from_tm(&tm))
    }

    #[cfg(not(any(unix, windows)))]
    fn from_local(_secs: u64) -> Option<Self> {
        None
    }

    /// Map a libc broken-down local time onto the RTC fields. `tm_wday`
    /// already uses the 0 = Sunday convention the weekday register expects.
    #[cfg(any(unix, windows))]
    fn from_tm(tm: &libc::tm) -> Self {
        Self {
            year: (tm.tm_year + 1900) as u16,
            month: (tm.tm_mon + 1) as u8,
            day: tm.tm_mday as u8,
            weekday: tm.tm_wday as u8,
            hour: tm.tm_hour as u8,
            minute: tm.tm_min as u8,
            second: tm.tm_sec as u8,
        }
    }
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn read_reg(rtc: &mut Msm6242Rtc, reg: u8) -> u8 {
        rtc.read((reg as u64) * 4, 4) as u8
    }

    #[test]
    fn registers_expose_bcd_host_time() {
        let mut rtc = Msm6242Rtc::default();
        rtc.set_test_time(UNIX_EPOCH + Duration::from_secs(946_782_245));

        assert_eq!(read_reg(&mut rtc, 0x0), 5);
        assert_eq!(read_reg(&mut rtc, 0x1), 0);
        assert_eq!(read_reg(&mut rtc, 0x2), 4);
        assert_eq!(read_reg(&mut rtc, 0x3), 0);
        assert_eq!(read_reg(&mut rtc, 0x4), 3);
        assert_eq!(read_reg(&mut rtc, 0x5), 0);
        assert_eq!(read_reg(&mut rtc, 0x6), 2);
        assert_eq!(read_reg(&mut rtc, 0x7), 0);
        assert_eq!(read_reg(&mut rtc, 0x8), 1);
        assert_eq!(read_reg(&mut rtc, 0x9), 0);
        assert_eq!(read_reg(&mut rtc, 0xA), 0);
        assert_eq!(read_reg(&mut rtc, 0xB), 0);
        assert_eq!(read_reg(&mut rtc, 0xC), 0);
        assert_eq!(
            read_reg(&mut rtc, 0xF) & Msm6242Rtc::CF_24H,
            Msm6242Rtc::CF_24H
        );
    }

    #[test]
    fn hold_write_latches_time_without_setting_host_clock() {
        let mut rtc = Msm6242Rtc::default();
        rtc.set_test_time(UNIX_EPOCH + Duration::from_secs(946_782_245));
        rtc.write(0xD * 4, 4, Msm6242Rtc::CD_HOLD as u64);
        assert_eq!(
            read_reg(&mut rtc, 0xD) & Msm6242Rtc::CD_HOLD,
            Msm6242Rtc::CD_HOLD
        );

        rtc.set_test_time(UNIX_EPOCH + Duration::from_secs(946_782_245 + 55));
        assert_eq!(read_reg(&mut rtc, 0x0), 5);

        rtc.write(0xD * 4, 4, 0);
        assert_eq!(read_reg(&mut rtc, 0x0), 0);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn live_path_uses_local_time() {
        // 2000-01-02 03:04:05 UTC stays within 2000-01-01..02 across every
        // real time zone (offsets are within +-14h), so the local
        // decomposition always lands in that window regardless of the test
        // host.
        let dt =
            RtcDateTime::from_system_time_local(UNIX_EPOCH + Duration::from_secs(946_782_245));
        assert_eq!(dt.year, 2000);
        assert_eq!(dt.month, 1);
        assert!(dt.day == 1 || dt.day == 2);
    }
}
