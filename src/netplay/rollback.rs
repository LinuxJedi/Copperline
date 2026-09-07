// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded, frame-indexed input prediction and rollback, independent of transport.

use super::Input;
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, VecDeque};

const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
pub(super) const HASH_INTERVAL: u64 = 60;

pub(super) trait Machine {
    fn save(&self) -> Result<Vec<u8>>;
    fn load(&mut self, state: &[u8]) -> Result<()>;
    fn frame(&mut self, inputs: [Input; 2], previous_keys: [u8; 16], replay: bool) -> Result<()>;
}

struct Frame {
    number: u64,
    before: Vec<u8>,
    remote: Input,
    previous_keys: [u8; 16],
}

pub(super) struct Rollback {
    pub current: u64,
    pub confirmed: u64,
    pub received: u64,
    pub acknowledged: u64,
    pub local: BTreeMap<u64, Input>,
    remote: BTreeMap<u64, Input>,
    history: VecDeque<Frame>,
    previous_keys: [u8; 16],
    dirty: Option<u64>,
    bytes: usize,
    player: usize,
    delay: u64,
    window: u64,
    pub hashes: BTreeMap<u64, [u8; 32]>,
    pub rollbacks: u64,
    pub replayed_frames: u64,
}

impl Rollback {
    pub fn new(player: usize, delay: u8, window: u8) -> Self {
        let neutral: BTreeMap<_, _> = (0..u64::from(delay))
            .map(|f| (f, Input::default()))
            .collect();
        Self {
            current: 0,
            confirmed: 0,
            received: u64::from(delay),
            acknowledged: u64::from(delay),
            local: neutral.clone(),
            remote: neutral,
            history: VecDeque::new(),
            previous_keys: [0; 16],
            dirty: None,
            bytes: 0,
            player,
            delay: u64::from(delay),
            window: u64::from(window),
            hashes: BTreeMap::new(),
            rollbacks: 0,
            replayed_frames: 0,
        }
    }

    pub fn receive(&mut self, frame: u64, input: Input) -> Result<()> {
        // Our submitted input reaches current + delay, allowing the peer's
        // frontier to reach current + delay + 1. It can then predict `window`
        // frames and sample one more delayed input while stalled there.
        ensure!(
            frame <= self.current + self.window + 2 * self.delay + 1,
            "netplay input is too far in the future"
        );
        if let Some(old) = self.remote.get(&frame) {
            ensure!(
                *old == input,
                "peer changed previously submitted input at frame {frame}"
            );
            return Ok(());
        }
        if frame < self.confirmed {
            return Ok(());
        }
        self.remote.insert(frame, input);
        if self
            .history
            .iter()
            .any(|f| f.number == frame && f.remote != input)
        {
            self.dirty = Some(self.dirty.map_or(frame, |old| old.min(frame)));
        }
        while self.remote.contains_key(&self.received) {
            self.received += 1;
        }
        Ok(())
    }

    pub fn acknowledge(&mut self, next: u64) -> Result<()> {
        let sent_end = self
            .local
            .last_key_value()
            .map_or(self.acknowledged, |(f, _)| f + 1);
        ensure!(
            next <= sent_end,
            "peer acknowledged input that was never submitted"
        );
        self.acknowledged = self.acknowledged.max(next);
        Ok(())
    }

    fn simulate(&mut self, machine: &mut impl Machine, number: u64, replay: bool) -> Result<()> {
        let remote = self.remote.range(..=number).next_back().map_or(
            Input::default(),
            |(&frame, &input)| {
                if frame == number {
                    input
                } else {
                    input.without_motion()
                }
            },
        );
        let local = *self
            .local
            .get(&number)
            .expect("local input exists before emulation");
        let before = machine.save()?;
        ensure!(self.bytes + before.len() <= MEMORY_LIMIT, "netplay snapshots exceed the 256 MiB memory budget; use less RAM or a smaller rollback window");
        let mut inputs = [remote; 2];
        inputs[self.player] = local;
        machine.frame(inputs, self.previous_keys, replay)?;
        self.history.push_back(Frame {
            number,
            before,
            remote,
            previous_keys: self.previous_keys,
        });
        self.bytes += self.history.back().unwrap().before.len();
        self.previous_keys = Input::merged_keys(inputs);
        Ok(())
    }

    pub fn reconcile(&mut self, machine: &mut impl Machine) -> Result<()> {
        if let Some(first) = self.dirty.take() {
            let index = self
                .history
                .iter()
                .position(|f| f.number == first)
                .expect("unconfirmed frame retained");
            machine.load(&self.history[index].before)?;
            self.previous_keys = self.history[index].previous_keys;
            while self.history.len() > index {
                self.bytes -= self.history.pop_back().unwrap().before.len();
            }
            for frame in first..self.current {
                self.simulate(machine, frame, true)?;
            }
            self.rollbacks += 1;
            self.replayed_frames += self.current - first;
        }
        self.confirm(machine)
    }

    fn confirm(&mut self, machine: &impl Machine) -> Result<()> {
        let end = self.current.min(self.received);
        let mut checkpoint = (self.confirmed / HASH_INTERVAL + 1) * HASH_INTERVAL;
        while checkpoint <= end {
            let digest = if checkpoint == self.current {
                super::digest(&machine.save()?)
            } else {
                let state = &self
                    .history
                    .iter()
                    .find(|f| f.number == checkpoint)
                    .expect("checkpoint retained")
                    .before;
                super::digest(state)
            };
            self.hashes.insert(checkpoint, digest);
            checkpoint += HASH_INTERVAL;
        }
        self.confirmed = end;
        while self.history.front().is_some_and(|f| f.number < end) {
            self.bytes -= self.history.pop_front().unwrap().before.len();
        }
        // Keep one remote input as the seed for repeat-last prediction.
        self.remote.retain(|f, _| *f >= end.saturating_sub(1));
        self.local.retain(|f, _| *f >= end.min(self.acknowledged));
        while self.hashes.len() > 8 {
            self.hashes.pop_first();
        }
        Ok(())
    }

    pub fn submit_local(&mut self, input: Input) -> bool {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            self.local.entry(self.current + self.delay)
        {
            entry.insert(input);
            true
        } else {
            false
        }
    }

    pub fn advance(&mut self, machine: &mut impl Machine, input: Input) -> Result<bool> {
        self.submit_local(input);
        if self.current >= self.received + self.window
            || self.current >= self.acknowledged + self.window
        {
            return Ok(false);
        }
        self.simulate(machine, self.current, false)?;
        self.current += 1;
        self.confirm(machine)?;
        Ok(true)
    }
}
