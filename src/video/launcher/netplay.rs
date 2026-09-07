// SPDX-License-Identifier: GPL-3.0-or-later

//! Editable connection details belong to this launcher session, not a machine file.

use super::*;
use crate::netplay::Options;

#[derive(Debug, Clone)]
pub struct NetplaySetup {
    pub enabled: bool,
    pub bind: String,
    pub peer: String,
    pub player: usize,
    pub code: String,
    pub delay: u8,
    pub rollback: u8,
}

impl Default for NetplaySetup {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "0.0.0.0:19732".into(),
            peer: String::new(),
            player: 0,
            code: String::new(),
            delay: 2,
            rollback: 8,
        }
    }
}

impl From<&Options> for NetplaySetup {
    fn from(options: &Options) -> Self {
        Self {
            enabled: true,
            bind: options.bind.to_string(),
            peer: options.peer.to_string(),
            player: options.player,
            code: options.session.iter().map(|b| format!("{b:02x}")).collect(),
            delay: options.input_delay,
            rollback: options.rollback_frames,
        }
    }
}

impl NetplaySetup {
    pub fn options(&self) -> Result<Option<Options>> {
        use anyhow::Context;
        if !self.enabled {
            return Ok(None);
        }
        let options = Options {
            bind: self.bind.parse().context("Local address needs IP:port")?,
            peer: self.peer.parse().context("Peer address needs IP:port")?,
            player: self.player,
            session: crate::netplay::parse_session_id(&self.code)?,
            input_delay: self.delay,
            rollback_frames: self.rollback,
        };
        options.validate()?;
        Ok(Some(options))
    }

    pub fn new_code(&mut self) {
        // RandomState obtains fresh host seeds. This is a collision-resistant
        // session label, not a secret or a peer-authentication credential.
        use std::hash::{BuildHasher, Hasher};
        let words: [u64; 2] = std::array::from_fn(|_| {
            let mut hash = std::collections::hash_map::RandomState::new().build_hasher();
            hash.write(b"Copperline netplay session");
            hash.finish()
        });
        self.code = format!("{:016x}{:016x}", words[0], words[1]);
    }

    pub fn value(&self, field: F) -> String {
        match field {
            F::NetplayBind => self.bind.clone(),
            F::NetplayPeer => self.peer.clone(),
            F::NetplayCode => self.code.clone(),
            F::NetplayPlayer => format!("{} (port {})", self.player + 1, self.player + 1),
            F::NetplayDelay => format!("{} frames", self.delay),
            F::NetplayRollback => format!("{} frames", self.rollback),
            F::NetplayNewCode => "New code".into(),
            F::NetplayCopyCode => "Copy code".into(),
            _ => String::new(),
        }
    }

    pub fn cycle(&mut self, field: F, forward: bool) {
        match field {
            F::NetplayPlayer => self.player = 1 - self.player,
            F::NetplayDelay => {
                self.delay = cycle_slice(&[0, 1, 2, 3, 4, 5, 6], self.delay, forward)
            }
            F::NetplayRollback => {
                self.rollback = cycle_slice(
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                    self.rollback,
                    forward,
                )
            }
            _ => {}
        }
    }
}

impl LauncherField {
    pub fn is_netplay(self) -> bool {
        matches!(
            self,
            F::NetplayEnabled
                | F::NetplayBind
                | F::NetplayPeer
                | F::NetplayPlayer
                | F::NetplayCode
                | F::NetplayDelay
                | F::NetplayRollback
                | F::NetplayNewCode
                | F::NetplayCopyCode
        )
    }
}

impl LauncherState {
    pub fn toggle_netplay(&mut self) {
        self.netplay.enabled = !self.netplay.enabled;
        self.prepare_netplay_machine();
    }

    pub fn prepare_netplay_machine(&mut self) {
        if self.netplay.enabled {
            // These session requirements are visible on the other pages too.
            self.setup.serial_mode = SerialMode::Off;
            self.setup.jit = false;
            self.setup.power_on = true;
            self.setup.run_ahead_frames = 0;
            self.setup.warp_boot = false;
            self.setup.warp_until = None;
            for port in &mut self.setup.port_devices {
                if !matches!(
                    port,
                    PortDevice::Mouse | PortDevice::Joystick | PortDevice::Cd32Pad
                ) {
                    *port = PortDevice::Joystick;
                }
            }
        }
    }

    pub fn begin_edit_netplay(&mut self, field: F) {
        if !self.netplay.enabled
            || !matches!(field, F::NetplayBind | F::NetplayPeer | F::NetplayCode)
        {
            return;
        }
        self.edit_buffer = self.netplay.value(field);
        self.editing = Some(EditTarget::Netplay(field));
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    pub fn clear_netplay_edit(&mut self) {
        if matches!(self.editing, Some(EditTarget::Netplay(_))) {
            self.edit_buffer.clear();
            self.edit_caret = Caret::default();
        }
    }

    pub(super) fn commit_netplay_edit(&mut self, field: F) {
        let value = self.edit_buffer.trim().to_string();
        match field {
            F::NetplayBind => self.netplay.bind = value,
            F::NetplayPeer => self.netplay.peer = value,
            F::NetplayCode => self.netplay.code = value,
            _ => {}
        }
        self.editing = None;
        self.edit_buffer.clear();
    }
}
