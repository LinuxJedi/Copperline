// SPDX-License-Identifier: GPL-3.0-or-later

//! Best-effort host thread-scheduling priority.
//!
//! An interactive emulator has two latency-critical threads: the wall-clock
//! pacer (the main thread, where jitter shows up as frame stutter and audio
//! drift) and the audio callback (a glitch there is instantly audible). On a
//! busy desktop the OS scheduler can preempt either at the wrong moment. When
//! the user opts in (`[emulation] realtime_priority = true` or the
//! `COPPERLINE_REALTIME_PRIORITY` env var), Copperline asks the OS to schedule
//! those threads above normal.
//!
//! "Real-time priority" is portable in neither API nor semantics, so this is
//! deliberately best-effort: every call logs what it did and never fails the
//! run. The whole feature is off by default, so an unprivileged or sandboxed
//! launch behaves exactly as before.
//!
//! Per platform:
//! * **macOS** -- the pacer thread joins the `USER_INTERACTIVE` QoS class
//!   (`pthread_set_qos_class_self_np`), the idiomatic way for an app thread to
//!   ask for low-latency scheduling without elevated privileges. The audio
//!   callback is deliberately left untouched: Core Audio already runs it on a
//!   real-time thread, and pinning a QoS class onto it would only *demote* it.
//! * **Windows** -- `SetThreadPriority` raises the thread to `HIGHEST` (via
//!   the [`thread_priority`] crate); no privilege required. WASAPI's callback
//!   runs on a cpal-spawned thread, so it is raised too.
//! * **Linux / other Unix** -- raising priority needs privilege (an `rtprio`
//!   rlimit, `CAP_SYS_NICE`, or root). Without it the request is declined and
//!   the thread keeps normal scheduling; that is logged once and is not fatal.

use std::sync::atomic::{AtomicBool, Ordering};

/// Resolve whether realtime-like scheduling was requested: the
/// `COPPERLINE_REALTIME_PRIORITY` env var overrides the
/// `[emulation] realtime_priority` config for one run. A value of
/// `0`/`false`/`off`/`no` forces it off; any other value (including an empty
/// string, i.e. the bare variable) forces it on.
pub fn requested(from_config: bool) -> bool {
    match crate::envcfg::var("COPPERLINE_REALTIME_PRIORITY") {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => from_config,
    }
}

/// Elevate the pacer (main) thread, which runs the emulator core and the
/// wall-clock pacer. Best effort; logs the outcome. Because the pacer sleeps
/// between work chunks rather than spinning (see
/// `Emulator::sleep_until_realtime_device_time`), even the strongest
/// scheduling class it can land in still yields the CPU and cannot starve the
/// host.
pub fn elevate_pacer_thread() {
    elevate_current_thread("pacer");
}

/// Promote the calling thread -- the cpal audio callback -- the first time it
/// runs. Idempotent across callbacks via an internal latch, so the one-time
/// scheduling syscall happens on the first callback only and steady-state
/// audio output does no extra work.
pub fn promote_audio_thread_once() {
    static PROMOTED: AtomicBool = AtomicBool::new(false);
    if PROMOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        // Core Audio already hands this callback a real-time thread; joining a
        // QoS class here would only lower it, so leave it as the OS set it.
        log::info!("priority: audio thread left as-is (Core Audio runs it real-time)");
    }
    #[cfg(not(target_os = "macos"))]
    {
        elevate_current_thread("audio");
    }
}

#[cfg(target_os = "macos")]
fn elevate_current_thread(label: &str) {
    // QOS_CLASS_USER_INTERACTIVE is the highest standard QoS class and the
    // conventional way for a latency-sensitive UI/media thread to request
    // low-latency scheduling without privileges. A relative priority of 0
    // keeps the thread at the class's reference priority.
    let ret = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if ret == 0 {
        log::info!("priority: {label} thread joined the USER_INTERACTIVE QoS class");
    } else {
        log::warn!(
            "priority: could not raise {label} thread QoS \
             (pthread_set_qos_class_self_np returned {ret}); continuing at normal priority"
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn elevate_current_thread(label: &str) {
    use thread_priority::{set_current_thread_priority, ThreadPriority};
    // On Windows this maps to THREAD_PRIORITY_HIGHEST (no privilege needed).
    // On Linux/other Unix it raises to the top of the current scheduling
    // policy, which requires privilege to exceed normal; without it the call
    // returns an error that we log and shrug off.
    match set_current_thread_priority(ThreadPriority::Max) {
        Ok(()) => log::info!("priority: {label} thread elevated (ThreadPriority::Max)"),
        Err(e) => log::warn!(
            "priority: could not elevate {label} thread ({e:?}); \
             continuing at normal priority"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevating_threads_is_always_safe() {
        // Best effort and privilege-dependent: on an unprivileged host the
        // underlying syscall may decline, but the wrappers must always return
        // cleanly rather than panic. This exercises the real macOS QoS path
        // (and the thread-priority path elsewhere) on the test thread.
        elevate_pacer_thread();
        // The audio promotion latches after its first call; both the first
        // call and the latched no-op second call must be safe.
        promote_audio_thread_once();
        promote_audio_thread_once();
    }

    #[test]
    fn requested_passes_config_through_without_env_override() {
        // `requested` only consults the config value when the env override is
        // absent. The unit suite sets no COPPERLINE_* vars, so guard on that
        // (envcfg snapshots the environment once) to keep the test hermetic.
        if crate::envcfg::var("COPPERLINE_REALTIME_PRIORITY").is_none() {
            assert!(requested(true));
            assert!(!requested(false));
        }
    }
}
