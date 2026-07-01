// SPDX-License-Identifier: GPL-3.0-or-later

//! WASM-hosted functional Zorro board: an external plugin compiled to
//! `wasm32` that implements a board's behaviour, run under wasmtime.
//!
//! A plugin module exports its behaviour and imports a small set of host
//! services (see the ABI below); its entire mutable state lives in WebAssembly
//! linear memory, which is a flat byte array that snapshots and restores
//! exactly like the Amiga RAM the emulator already serializes. That is why WASM
//! fits Copperline's determinism / save-state contract.
//!
//! ## Module ABI
//!
//! Exports the plugin provides (all optional except `memory`):
//! - `memory`                       the linear memory (required)
//! - `init()`                       called once after instantiation
//! - `read(off: i32, size: i32) -> i32`   register read in the board window
//! - `write(off: i32, size: i32, value: i32)`  register write
//! - `tick(cck: i32)`               advance by `cck` colour clocks
//! - `int2() -> i32`                INT2 (PORTS) line state, 0/1
//! - `int6() -> i32`                INT6 (EXTER) line state, 0/1
//!
//! Imports the host provides in module `env` (capability-gated):
//! - `log(ptr: i32, len: i32)`                      always available
//! - `dma_read(addr: i32, ptr: i32, len: i32)`      requires the `dma` capability
//! - `dma_write(addr: i32, ptr: i32, len: i32)`     requires the `dma` capability
//!
//! `dma_read` copies `len` bytes from Amiga address `addr` into the plugin's
//! linear memory at `ptr`; `dma_write` copies the other way. Both use the
//! shared 24-bit chip/slow/Zorro decode in [`crate::zorro_device`].
//!
//! ## Determinism
//!
//! The wasmtime engine is configured for determinism (NaN canonicalization, no
//! SIMD/threads). Persistent mutable state must live in linear memory: snapshots
//! capture linear memory and its page count, not WebAssembly globals (the
//! shadow-stack pointer is unwound to a constant between calls, so it needs no
//! capture). Save-state replay of a plugin is only guaranteed within one
//! wasmtime build; the version is pinned in `Cargo.toml`.

use crate::memory::Memory;
use crate::net::{make_backend, NetBackend, NetConfig};
use crate::zorro_device::{DeviceHost, ZorroDevice};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use wasmtime::{
    Caller, Config, Engine, Extern, Linker, Memory as WasmMemory, Module, Store, TypedFunc,
};

/// Capabilities a plugin declares in its manifest; ungranted host imports are
/// not linked, so a module that needs more than it declared fails to
/// instantiate (loudly) rather than silently misbehaving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WasmCaps {
    /// Bus-master DMA into Amiga memory (`dma_read`/`dma_write` imports).
    pub dma: bool,
    /// Asserts the INT2 (PORTS) line (advisory; the `int2` export is polled).
    pub int2: bool,
    /// Asserts the INT6 (EXTER) line (advisory; the `int6` export is polled).
    pub int6: bool,
    /// Host networking (`net_send`/`net_recv` imports). A net board is
    /// non-deterministic; see [`crate::net`].
    pub net: bool,
}

/// A plugin's non-autoconfig metadata: its display name, capabilities, and (for
/// a NIC board) which host network backend to bring up. The autoconfig identity
/// lives in the board's [`crate::zorro::BoardSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WasmManifest {
    pub name: String,
    pub caps: WasmCaps,
    pub net: NetConfig,
    /// Effective plugin settings (manifest defaults merged with the user's
    /// per-board overrides), exposed to the module via the `config_get` import.
    pub config: BTreeMap<String, String>,
    /// Config keys whose values are host file paths; the host loads each file
    /// and exposes it to the module via `resource_read` under the same key.
    pub file_keys: Vec<String>,
}

/// Store data for host imports. The Amiga-memory pointer is stored as a
/// `usize` (0 = none) so the store stays `Send`; it is set to the live
/// `&mut Memory` only for the duration of a plugin call (see [`WasmRuntime::enter`]).
struct HostCtx {
    /// Address of the live `Memory`, valid only during a plugin call.
    mem: usize,
    name: String,
    /// Host network backend for a NIC plugin (the `net` capability). A host
    /// resource, not serialized: brought up fresh from the manifest's
    /// [`NetConfig`] on instantiation and reset.
    net: Option<Box<dyn NetBackend>>,
    /// Effective plugin settings, read by the `config_get` import.
    config: BTreeMap<String, String>,
    /// Loaded file resources, read by the `resource_*` imports.
    resources: HashMap<String, Vec<u8>>,
}

/// The typed entry points a plugin may export.
struct Exports {
    read: Option<TypedFunc<(i32, i32), i32>>,
    write: Option<TypedFunc<(i32, i32, i32), ()>>,
    tick: Option<TypedFunc<i32, ()>>,
    int2: Option<TypedFunc<(), i32>>,
    int6: Option<TypedFunc<(), i32>>,
}

/// The live wasmtime state for one plugin board. Holds the engine and compiled
/// module so a power-on reset can re-instantiate a fresh module (clearing the
/// plugin's RAM) without recompiling.
struct WasmRuntime {
    engine: Engine,
    module: Module,
    manifest: WasmManifest,
    /// File resources loaded once from the manifest's file-typed config values.
    resources: HashMap<String, Vec<u8>>,
    store: Store<HostCtx>,
    memory: WasmMemory,
    exports: Exports,
}

impl WasmRuntime {
    fn new(engine: Engine, module: Module, manifest: WasmManifest) -> Result<Self> {
        let resources = load_resources(&manifest)?;
        let (store, memory, exports) = Self::instantiate(&engine, &module, &manifest, &resources)?;
        Ok(Self {
            engine,
            module,
            manifest,
            resources,
            store,
            memory,
            exports,
        })
    }

    /// Build a fresh store/instance from an engine + compiled module.
    fn instantiate(
        engine: &Engine,
        module: &Module,
        manifest: &WasmManifest,
        resources: &HashMap<String, Vec<u8>>,
    ) -> Result<(Store<HostCtx>, WasmMemory, Exports)> {
        let mut store = Store::new(
            engine,
            HostCtx {
                mem: 0,
                name: manifest.name.clone(),
                net: make_backend(manifest.net),
                config: manifest.config.clone(),
                resources: resources.clone(),
            },
        );
        let mut linker = Linker::new(engine);
        register_host_fns(&mut linker, manifest.caps)?;
        let instance = linker
            .instantiate(&mut store, module)
            .context("instantiating WASM plugin")?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("WASM plugin exports no `memory`"))?;
        let exports = Exports {
            read: instance.get_typed_func(&mut store, "read").ok(),
            write: instance.get_typed_func(&mut store, "write").ok(),
            tick: instance.get_typed_func(&mut store, "tick").ok(),
            int2: instance.get_typed_func(&mut store, "int2").ok(),
            int6: instance.get_typed_func(&mut store, "int6").ok(),
        };
        if let Ok(init) = instance.get_typed_func::<(), ()>(&mut store, "init") {
            refuel(&mut store);
            init.call(&mut store, ())
                .context("WASM plugin init() trapped")?;
        }
        Ok((store, memory, exports))
    }

    /// Re-instantiate from the kept engine + module (cold reset: clears RAM).
    fn reset(&mut self) -> Result<()> {
        let (store, memory, exports) =
            Self::instantiate(&self.engine, &self.module, &self.manifest, &self.resources)?;
        self.store = store;
        self.memory = memory;
        self.exports = exports;
        Ok(())
    }

    /// Point the store at the live Amiga memory for the duration of a call.
    fn enter(&mut self, mem: &mut Memory) {
        self.store.data_mut().mem = mem as *mut Memory as usize;
    }

    /// Clear the Amiga-memory pointer once a call returns.
    fn leave(&mut self) {
        self.store.data_mut().mem = 0;
    }

    /// Snapshot linear memory and its current page count.
    fn snapshot(&mut self) -> (u64, Vec<u8>) {
        let pages = self.memory.size(&self.store);
        let bytes = self.memory.data(&self.store).to_vec();
        (pages, bytes)
    }

    /// Restore a snapshot: grow linear memory to the saved page count, then
    /// write the saved bytes back.
    fn restore(&mut self, pages: u64, bytes: &[u8]) -> Result<()> {
        let cur = self.memory.size(&self.store);
        if pages > cur {
            self.memory
                .grow(&mut self.store, pages - cur)
                .context("growing WASM plugin memory on restore")?;
        }
        self.memory
            .write(&mut self.store, 0, bytes)
            .context("restoring WASM plugin memory")?;
        Ok(())
    }
}

/// Load the file resources a manifest's file-typed config values name. Like the
/// module itself and HDF/CD images, these are reopened by path (here and again
/// on a save-state load), not carried in the snapshot.
fn load_resources(manifest: &WasmManifest) -> Result<HashMap<String, Vec<u8>>> {
    let mut map = HashMap::new();
    for key in &manifest.file_keys {
        match manifest.config.get(key) {
            Some(path) if !path.is_empty() => {
                let bytes = std::fs::read(path)
                    .with_context(|| format!("loading WASM plugin resource {key:?} from {path}"))?;
                map.insert(key.clone(), bytes);
            }
            _ => {} // an unset file option is simply absent
        }
    }
    Ok(map)
}

/// Fuel budget refilled before every entry into a plugin. Copperline runs
/// synchronously on the main emulation thread (see CLAUDE.md), so a plugin
/// with a runaway loop in `init`/`read`/`write`/`tick`/`int2`/`int6` would
/// otherwise hang the whole process forever with no recovery path. This
/// bounds a single call to a large but finite instruction budget instead;
/// exhausting it surfaces as an ordinary trap through the existing
/// log-and-fall-back-to-default handling at each call site.
const PLUGIN_FUEL_BUDGET: u64 = 50_000_000;

/// Refill a store's fuel to [`PLUGIN_FUEL_BUDGET`] ahead of a plugin call.
/// Only fails if fuel consumption isn't configured on the engine, which
/// `make_engine` always enables.
fn refuel(store: &mut Store<HostCtx>) {
    store
        .set_fuel(PLUGIN_FUEL_BUDGET)
        .expect("engine always configures consume_fuel(true)");
}

/// Build the deterministic wasmtime engine. NaN canonicalization removes
/// host-CPU NaN bit-pattern leakage; SIMD/relaxed-SIMD/threads are disabled
/// (relaxed-SIMD is nondeterministic by spec, threads add shared-memory
/// nondeterminism). Fuel consumption caps how long any single call into a
/// plugin can run (see [`PLUGIN_FUEL_BUDGET`]).
fn make_engine() -> Result<Engine> {
    let mut cfg = Config::new();
    cfg.cranelift_nan_canonicalization(true);
    cfg.wasm_simd(false);
    cfg.wasm_relaxed_simd(false);
    cfg.consume_fuel(true);
    // The `threads` wasmtime feature is not built in (see Cargo.toml), so shared
    // memory / atomics are unavailable -- no separate knob needed.
    Engine::new(&cfg).context("creating WASM engine")
}

/// Register the host imports the plugin may call, gated by capability.
fn register_host_fns(linker: &mut Linker<HostCtx>, caps: WasmCaps) -> Result<()> {
    // log(ptr, len): always available.
    linker.func_wrap(
        "env",
        "log",
        |mut caller: Caller<'_, HostCtx>, ptr: i32, len: i32| -> Result<()> {
            let buf = read_wasm_bytes(&mut caller, ptr, len)?;
            let name = caller.data().name.clone();
            log::info!("wasm[{name}]: {}", String::from_utf8_lossy(&buf));
            Ok(())
        },
    )?;

    // config_get(key_ptr, key_len, out_ptr, out_cap) -> i32: copy the setting's
    // value into linear memory (truncated to out_cap) and return its full
    // length, or -1 if the key is absent. Always available.
    linker.func_wrap(
        "env",
        "config_get",
        |mut caller: Caller<'_, HostCtx>,
         key_ptr: i32,
         key_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> Result<i32> {
            let key = read_wasm_bytes(&mut caller, key_ptr, key_len)?;
            let key = String::from_utf8_lossy(&key).into_owned();
            let Some(value) = caller.data().config.get(&key).cloned() else {
                return Ok(-1);
            };
            let bytes = value.as_bytes();
            let n = bytes.len().min(out_cap.max(0) as usize);
            write_wasm_bytes(&mut caller, out_ptr, &bytes[..n])?;
            Ok(bytes.len() as i32)
        },
    )?;

    // resource_len(name_ptr, name_len) -> i32: byte length of a file resource,
    // or -1 if absent.
    linker.func_wrap(
        "env",
        "resource_len",
        |mut caller: Caller<'_, HostCtx>, name_ptr: i32, name_len: i32| -> Result<i32> {
            let name = read_wasm_bytes(&mut caller, name_ptr, name_len)?;
            let name = String::from_utf8_lossy(&name).into_owned();
            Ok(caller
                .data()
                .resources
                .get(&name)
                .map(|b| b.len() as i32)
                .unwrap_or(-1))
        },
    )?;

    // resource_read(name_ptr, name_len, off, out_ptr, len) -> i32: copy
    // resource[off..off+len] into linear memory; returns the byte count, or -1
    // if the resource is absent.
    linker.func_wrap(
        "env",
        "resource_read",
        |mut caller: Caller<'_, HostCtx>,
         name_ptr: i32,
         name_len: i32,
         off: i32,
         out_ptr: i32,
         len: i32|
         -> Result<i32> {
            let name = read_wasm_bytes(&mut caller, name_ptr, name_len)?;
            let name = String::from_utf8_lossy(&name).into_owned();
            let chunk = match caller.data().resources.get(&name) {
                Some(bytes) => {
                    let off = off.max(0) as usize;
                    let end = off.saturating_add(len.max(0) as usize).min(bytes.len());
                    if off >= bytes.len() {
                        Vec::new()
                    } else {
                        bytes[off..end].to_vec()
                    }
                }
                None => return Ok(-1),
            };
            write_wasm_bytes(&mut caller, out_ptr, &chunk)?;
            Ok(chunk.len() as i32)
        },
    )?;

    if caps.dma {
        // dma_read(addr, ptr, len): Amiga[addr..] -> wasm linear memory[ptr..].
        linker.func_wrap(
            "env",
            "dma_read",
            |mut caller: Caller<'_, HostCtx>, addr: i32, ptr: i32, len: i32| -> Result<()> {
                // Validate the destination window before allocating, so a
                // plugin-controlled `len` can't force an oversized host
                // allocation (see `checked_wasm_window`). `write_wasm_bytes`
                // below re-validates the same window when it stores `buf`.
                let mem_size = caller_memory(&mut caller)?.data_size(&caller);
                let (_, len) = checked_wasm_window(ptr, len, mem_size)?;
                let mut buf = vec![0u8; len];
                with_amiga_memory(&caller, |amiga| {
                    DeviceHost::new(amiga).dma_read(addr as u32, &mut buf);
                });
                write_wasm_bytes(&mut caller, ptr, &buf)
            },
        )?;

        // dma_write(addr, ptr, len): wasm linear memory[ptr..] -> Amiga[addr..].
        linker.func_wrap(
            "env",
            "dma_write",
            |mut caller: Caller<'_, HostCtx>, addr: i32, ptr: i32, len: i32| -> Result<()> {
                let buf = read_wasm_bytes(&mut caller, ptr, len)?;
                with_amiga_memory(&caller, |amiga| {
                    DeviceHost::new(amiga).dma_write(addr as u32, &buf);
                });
                Ok(())
            },
        )?;
    }

    if caps.net {
        // net_send(ptr, len): transmit the Ethernet frame in linear memory.
        linker.func_wrap(
            "env",
            "net_send",
            |mut caller: Caller<'_, HostCtx>, ptr: i32, len: i32| -> Result<()> {
                let frame = read_wasm_bytes(&mut caller, ptr, len)?;
                if let Some(net) = caller.data_mut().net.as_mut() {
                    net.send(&frame);
                }
                Ok(())
            },
        )?;

        // net_recv(ptr, cap) -> i32: copy the next inbound frame into linear
        // memory at `ptr` (truncated to `cap` bytes) and return its length, or
        // 0 when none is waiting.
        linker.func_wrap(
            "env",
            "net_recv",
            |mut caller: Caller<'_, HostCtx>, ptr: i32, cap: i32| -> Result<i32> {
                let Some(frame) = caller.data_mut().net.as_mut().and_then(|n| n.poll()) else {
                    return Ok(0);
                };
                let n = frame.len().min(cap.max(0) as usize);
                write_wasm_bytes(&mut caller, ptr, &frame[..n])?;
                Ok(n as i32)
            },
        )?;
    }
    Ok(())
}

/// Run `f` with the live Amiga memory the store currently points at. No-op when
/// the pointer is unset (a plugin should only DMA from within a host call).
fn with_amiga_memory(caller: &Caller<'_, HostCtx>, f: impl FnOnce(&mut Memory)) {
    let mem = caller.data().mem;
    if mem == 0 {
        return;
    }
    // SAFETY: `mem` is the address of the `&mut Memory` set by `WasmRuntime::enter`
    // for the duration of this plugin call (see the ZorroDevice impl below). It is
    // not aliased while the plugin runs -- the outer DeviceHost is not touched
    // until the call returns -- and is cleared to 0 afterwards.
    let amiga = unsafe { &mut *(mem as *mut Memory) };
    f(amiga);
}

/// Validate a plugin-supplied `[ptr, ptr+len)` window against the plugin's
/// current linear-memory size, returning the clamped `(ptr, len)`. `ptr`
/// and `len` are plugin-controlled and unrelated to how much memory the
/// plugin actually has, so an out-of-range window is rejected *before* the
/// host allocates `len` bytes -- otherwise a huge `len` (up to ~2 GiB via
/// i32) would force an oversized host allocation per call. An out-of-range
/// window is a clean error, exactly as the previous trap-on-access was.
fn checked_wasm_window(ptr: i32, len: i32, mem_size: usize) -> Result<(usize, usize)> {
    let ptr = ptr.max(0) as usize;
    let len = len.max(0) as usize;
    if ptr.checked_add(len).is_none_or(|end| end > mem_size) {
        anyhow::bail!("WASM plugin memory window {ptr}+{len} exceeds {mem_size} bytes");
    }
    Ok((ptr, len))
}

/// Read `len` bytes from the plugin's linear memory at `ptr`.
fn read_wasm_bytes(caller: &mut Caller<'_, HostCtx>, ptr: i32, len: i32) -> Result<Vec<u8>> {
    let memory = caller_memory(caller)?;
    let (ptr, len) = checked_wasm_window(ptr, len, memory.data_size(&caller))?;
    let mut buf = vec![0u8; len];
    memory
        .read(&mut *caller, ptr, &mut buf)
        .context("reading WASM plugin memory")?;
    Ok(buf)
}

/// Write `buf` into the plugin's linear memory at `ptr`.
fn write_wasm_bytes(caller: &mut Caller<'_, HostCtx>, ptr: i32, buf: &[u8]) -> Result<()> {
    let memory = caller_memory(caller)?;
    memory
        .write(&mut *caller, ptr.max(0) as usize, buf)
        .context("writing WASM plugin memory")?;
    Ok(())
}

fn caller_memory(caller: &mut Caller<'_, HostCtx>) -> Result<WasmMemory> {
    caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| anyhow!("WASM plugin exports no `memory`"))
}

/// A functional Zorro board implemented by a WASM plugin module.
///
/// Serializes via a path-reopen shadow (like HDF/CD images): the snapshot
/// carries the module path, manifest, and a linear-memory image; on load the
/// module is recompiled from its path and the image replayed.
pub struct WasmBoard {
    module_path: PathBuf,
    rt: RefCell<WasmRuntime>,
}

impl WasmBoard {
    /// Load and instantiate a plugin module from a `.wasm` file.
    pub fn from_file(path: &Path, manifest: WasmManifest) -> Result<Self> {
        if manifest.net != NetConfig::None {
            log::warn!(
                "wasm[{}]: network backend {:?} active -- deterministic replay \
                 and save-state reproducibility are not guaranteed while \
                 traffic flows",
                manifest.name,
                manifest.net
            );
        }
        let engine = make_engine()?;
        let module = Module::from_file(&engine, path)
            .with_context(|| format!("compiling WASM plugin {}", path.display()))?;
        let rt = WasmRuntime::new(engine, module, manifest)?;
        Ok(Self {
            module_path: path.to_path_buf(),
            rt: RefCell::new(rt),
        })
    }

    /// Call an exported function that takes no Amiga memory, returning its
    /// `i32` result (0 if the export is absent or traps).
    fn call_flag(&self, sel: impl FnOnce(&Exports) -> Option<TypedFunc<(), i32>>) -> bool {
        let mut rt = self.rt.borrow_mut();
        let Some(func) = sel(&rt.exports) else {
            return false;
        };
        refuel(&mut rt.store);
        match func.call(&mut rt.store, ()) {
            Ok(v) => v != 0,
            Err(e) => {
                log::warn!("wasm[{}]: int line query trapped: {e}", rt.manifest.name);
                false
            }
        }
    }
}

impl ZorroDevice for WasmBoard {
    fn read(&mut self, off: u32, size: usize, host: &mut DeviceHost) -> u32 {
        let rt = self.rt.get_mut();
        let Some(func) = rt.exports.read.clone() else {
            return 0xFFFF_FFFF;
        };
        rt.enter(host.memory_mut());
        refuel(&mut rt.store);
        let result = func.call(&mut rt.store, (off as i32, size as i32));
        rt.leave();
        match result {
            Ok(v) => v as u32,
            Err(e) => {
                log::warn!("wasm[{}]: read trapped: {e}", rt.manifest.name);
                0xFFFF_FFFF
            }
        }
    }

    fn write(&mut self, off: u32, size: usize, value: u32, host: &mut DeviceHost) {
        let rt = self.rt.get_mut();
        let Some(func) = rt.exports.write.clone() else {
            return;
        };
        rt.enter(host.memory_mut());
        refuel(&mut rt.store);
        let result = func.call(&mut rt.store, (off as i32, size as i32, value as i32));
        rt.leave();
        if let Err(e) = result {
            log::warn!("wasm[{}]: write trapped: {e}", rt.manifest.name);
        }
    }

    fn tick(&mut self, cck: u32, host: &mut DeviceHost) {
        let rt = self.rt.get_mut();
        let Some(func) = rt.exports.tick.clone() else {
            return;
        };
        rt.enter(host.memory_mut());
        refuel(&mut rt.store);
        let result = func.call(&mut rt.store, cck as i32);
        rt.leave();
        if let Err(e) = result {
            log::warn!("wasm[{}]: tick trapped: {e}", rt.manifest.name);
        }
    }

    fn int2_line(&self) -> bool {
        self.call_flag(|e| e.int2.clone())
    }

    fn int6_line(&self) -> bool {
        self.call_flag(|e| e.int6.clone())
    }

    // Ticked every slice (the plugin decides what to do); a sparse next_event
    // model is a future optimization.
    fn is_idle(&self) -> bool {
        false
    }

    fn reset(&mut self) {
        if let Err(e) = self.rt.get_mut().reset() {
            log::error!("wasm: plugin reset failed: {e}");
        }
    }

    fn kind(&self) -> &'static str {
        "wasm"
    }
}

/// The serialized form of a [`WasmBoard`]: enough to recompile the module from
/// its path and replay its linear memory.
#[derive(Serialize, Deserialize)]
struct WasmBoardState {
    module_path: PathBuf,
    manifest: WasmManifest,
    pages: u64,
    bytes: Vec<u8>,
}

impl Serialize for WasmBoard {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut rt = self.rt.borrow_mut();
        let (pages, bytes) = rt.snapshot();
        let state = WasmBoardState {
            module_path: self.module_path.clone(),
            manifest: rt.manifest.clone(),
            pages,
            bytes,
        };
        state.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WasmBoard {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = WasmBoardState::deserialize(deserializer)?;
        let board = WasmBoard::from_file(&state.module_path, state.manifest)
            .map_err(serde::de::Error::custom)?;
        board
            .rt
            .borrow_mut()
            .restore(state.pages, &state.bytes)
            .map_err(serde::de::Error::custom)?;
        Ok(board)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A golden test plugin: a 16-bit counter at window offset 0, incremented
    /// by `tick`, readable/writable, asserting INT2 once it passes a threshold.
    /// Its whole state is one i32 in linear memory at address 0.
    const COUNTER_WAT: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "read") (param $off i32) (param $size i32) (result i32)
            (i32.load (i32.const 0)))
          (func (export "write") (param $off i32) (param $size i32) (param $val i32)
            (i32.store (i32.const 0) (local.get $val)))
          (func (export "tick") (param $cck i32)
            (i32.store (i32.const 0)
              (i32.add (i32.load (i32.const 0)) (i32.const 1))))
          (func (export "int2") (result i32)
            (i32.gt_u (i32.load (i32.const 0)) (i32.const 3)))
        )
    "#;

    /// A DMA test plugin: `write(off, size, val)` reads 4 bytes from Amiga
    /// address `val` into linear memory and stores their big-endian sum back so
    /// `read` returns it; exercises the dma_read host import.
    const DMA_WAT: &str = r#"
        (module
          (import "env" "dma_read" (func $dma_read (param i32 i32 i32)))
          (memory (export "memory") 1)
          (func (export "read") (param $off i32) (param $size i32) (result i32)
            (i32.load (i32.const 0)))
          (func (export "write") (param $off i32) (param $size i32) (param $addr i32)
            ;; copy 4 bytes from Amiga[$addr] into linear memory at offset 16
            (call $dma_read (local.get $addr) (i32.const 16) (i32.const 4))
            ;; store the 32-bit big-endian value at offset 0
            (i32.store (i32.const 0)
              (i32.or
                (i32.or
                  (i32.shl (i32.load8_u (i32.const 16)) (i32.const 24))
                  (i32.shl (i32.load8_u (i32.const 17)) (i32.const 16)))
                (i32.or
                  (i32.shl (i32.load8_u (i32.const 18)) (i32.const 8))
                  (i32.load8_u (i32.const 19))))))
        )
    "#;

    fn write_wasm(name: &str, wat: &str) -> PathBuf {
        let bytes = wat::parse_str(wat).expect("valid WAT");
        let path = std::env::temp_dir().join(format!(
            "copperline-wasmboard-{name}-{}-{}.wasm",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &bytes).expect("write wasm");
        path
    }

    fn manifest(name: &str, dma: bool) -> WasmManifest {
        WasmManifest {
            name: name.into(),
            caps: WasmCaps {
                dma,
                int2: true,
                int6: false,
                net: false,
            },
            net: NetConfig::None,
            config: BTreeMap::new(),
            file_keys: Vec::new(),
        }
    }

    fn net_manifest(name: &str) -> WasmManifest {
        WasmManifest {
            name: name.into(),
            caps: WasmCaps {
                dma: false,
                int2: false,
                int6: false,
                net: true,
            },
            net: NetConfig::Loopback,
            config: BTreeMap::new(),
            file_keys: Vec::new(),
        }
    }

    fn empty_memory() -> Memory {
        Memory {
            chip_ram: vec![0u8; 0x1000],
            slow_ram: Vec::new(),
            rom: Vec::new(),
            overlay: false,
            zorro: crate::zorro::ZorroChain::default(),
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs: Vec::new(),
            wcs_write_protected: false,
        }
    }

    #[test]
    fn counter_plugin_reads_writes_and_ticks() {
        let path = write_wasm("counter", COUNTER_WAT);
        let mut board = WasmBoard::from_file(&path, manifest("counter", false)).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);

        assert_eq!(board.read(0, 2, &mut host), 0);
        board.write(0, 2, 10, &mut host);
        assert_eq!(board.read(0, 2, &mut host), 10);

        // tick increments; int2 asserts once the counter passes 3.
        let mut fresh = WasmBoard::from_file(&path, manifest("counter", false)).unwrap();
        assert!(!fresh.int2_line());
        for _ in 0..5 {
            fresh.tick(1, &mut host);
        }
        assert_eq!(fresh.read(0, 2, &mut host), 5);
        assert!(fresh.int2_line());

        let _ = std::fs::remove_file(&path);
    }

    /// A runaway plugin: `tick` never returns. Copperline runs synchronously
    /// on the main thread, so without a fuel cap this would hang the whole
    /// emulator forever instead of trapping.
    const INFINITE_LOOP_WAT: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "tick") (param $cck i32)
            (loop $forever
              (br $forever)))
        )
    "#;

    #[test]
    fn runaway_plugin_loop_traps_on_fuel_instead_of_hanging() {
        let path = write_wasm("infinite_loop", INFINITE_LOOP_WAT);
        let mut board = WasmBoard::from_file(&path, manifest("infinite_loop", false)).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);

        // Must return (the fuel budget traps the loop) rather than hang;
        // the test process itself would time out if it didn't.
        board.tick(1, &mut host);

        let _ = std::fs::remove_file(&path);
    }

    /// `write(off, size, len)` calls `dma_read` with a caller-supplied
    /// length instead of a fixed one, to exercise an oversized `len`.
    const DMA_OVERSIZED_LEN_WAT: &str = r#"
        (module
          (import "env" "dma_read" (func $dma_read (param i32 i32 i32)))
          (memory (export "memory") 1)
          (func (export "write") (param $off i32) (param $size i32) (param $len i32)
            (call $dma_read (i32.const 0) (i32.const 0) (local.get $len)))
        )
    "#;

    #[test]
    fn dma_read_with_oversized_len_does_not_allocate_unbounded_memory() {
        let path = write_wasm("dma_oversized", DMA_OVERSIZED_LEN_WAT);
        let mut board = WasmBoard::from_file(&path, manifest("dma_oversized", true)).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);

        // len = i32::MAX: without capping the allocation to the plugin's
        // actual (1-page = 64 KiB) linear memory, this would attempt a
        // ~2 GiB host allocation on every call.
        board.write(0, 0, i32::MAX as u32, &mut host);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reset_clears_plugin_memory() {
        let path = write_wasm("counter", COUNTER_WAT);
        let mut board = WasmBoard::from_file(&path, manifest("counter", false)).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);

        board.write(0, 2, 42, &mut host);
        assert_eq!(board.read(0, 2, &mut host), 42);
        board.reset();
        assert_eq!(board.read(0, 2, &mut host), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_state_round_trip_is_byte_identical() {
        let path = write_wasm("counter", COUNTER_WAT);
        let mut board = WasmBoard::from_file(&path, manifest("counter", false)).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);
        board.write(0, 2, 99, &mut host);

        // Serialize, mutate the live board, then restore from the snapshot.
        let blob = bincode::serialize(&board).unwrap();
        board.write(0, 2, 7, &mut host);
        assert_eq!(board.read(0, 2, &mut host), 7);

        let restored: WasmBoard = bincode::deserialize(&blob).unwrap();
        let mut rmem = empty_memory();
        let mut rhost = DeviceHost::new(&mut rmem);
        let mut restored = restored;
        assert_eq!(restored.read(0, 2, &mut rhost), 99);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn memory_grow_round_trips_through_a_snapshot() {
        // A plugin that grows its memory then writes a marker high up; the
        // snapshot must capture the grown page count and the marker.
        let grow_wat = r#"
            (module
              (memory (export "memory") 1)
              (func (export "write") (param i32 i32 i32)
                (drop (memory.grow (i32.const 1)))
                (i32.store (i32.const 65540) (i32.const 12345)))
              (func (export "read") (param i32 i32) (result i32)
                (i32.load (i32.const 65540)))
            )
        "#;
        let path = write_wasm("grow", grow_wat);
        let mut board = WasmBoard::from_file(&path, manifest("grow", false)).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);
        board.write(0, 0, 0, &mut host); // grow + write marker in the new page
        assert_eq!(board.read(0, 0, &mut host), 12345);

        let blob = bincode::serialize(&board).unwrap();
        let mut restored: WasmBoard = bincode::deserialize(&blob).unwrap();
        let mut rmem = empty_memory();
        let mut rhost = DeviceHost::new(&mut rmem);
        assert_eq!(restored.read(0, 0, &mut rhost), 12345);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dma_read_import_reaches_amiga_chip_ram() {
        let path = write_wasm("dma", DMA_WAT);
        let mut board = WasmBoard::from_file(&path, manifest("dma", true)).unwrap();
        let mut mem = empty_memory();
        mem.chip_ram[0x40] = 0xDE;
        mem.chip_ram[0x41] = 0xAD;
        mem.chip_ram[0x42] = 0xBE;
        mem.chip_ram[0x43] = 0xEF;
        let mut host = DeviceHost::new(&mut mem);

        // write() triggers dma_read of 4 bytes from Amiga $40.
        board.write(0, 0, 0x40, &mut host);
        assert_eq!(board.read(0, 4, &mut host), 0xDEAD_BEEF);

        let _ = std::fs::remove_file(&path);
    }

    /// A NIC test plugin: `write(_, _, val)` transmits a 4-byte frame
    /// `[val, AA, BB, CC]`; `read` polls a frame into linear memory and returns
    /// `(len << 16) | first_byte`. With the loopback backend, what is sent
    /// comes straight back.
    const NET_WAT: &str = r#"
        (module
          (import "env" "net_send" (func $net_send (param i32 i32)))
          (import "env" "net_recv" (func $net_recv (param i32 i32) (result i32)))
          (memory (export "memory") 1)
          (func (export "write") (param $off i32) (param $size i32) (param $val i32)
            (i32.store8 (i32.const 32) (local.get $val))
            (i32.store8 (i32.const 33) (i32.const 0xAA))
            (i32.store8 (i32.const 34) (i32.const 0xBB))
            (i32.store8 (i32.const 35) (i32.const 0xCC))
            (call $net_send (i32.const 32) (i32.const 4)))
          (func (export "read") (param $off i32) (param $size i32) (result i32)
            (local $n i32)
            (local.set $n (call $net_recv (i32.const 64) (i32.const 128)))
            (i32.or
              (i32.shl (local.get $n) (i32.const 16))
              (i32.load8_u (i32.const 64))))
        )
    "#;

    /// A plugin that reads a setting and a file resource at init: `init` puts
    /// the config value's first byte at mem[256], the resource length at
    /// mem[257], and the resource's first 4 bytes at mem[258..]; `read(off)`
    /// returns mem[256 + off].
    const CONFIG_WAT: &str = r#"
        (module
          (import "env" "config_get" (func $config_get (param i32 i32 i32 i32) (result i32)))
          (import "env" "resource_len" (func $resource_len (param i32 i32) (result i32)))
          (import "env" "resource_read" (func $resource_read (param i32 i32 i32 i32 i32) (result i32)))
          (memory (export "memory") 1)
          (data (i32.const 0) "buffers")
          (data (i32.const 16) "rom")
          (func (export "init")
            (drop (call $config_get (i32.const 0) (i32.const 7) (i32.const 256) (i32.const 64)))
            (i32.store8 (i32.const 257) (call $resource_len (i32.const 16) (i32.const 3)))
            (drop (call $resource_read (i32.const 16) (i32.const 3) (i32.const 0) (i32.const 258) (i32.const 4))))
          (func (export "read") (param $off i32) (param $size i32) (result i32)
            (i32.load8_u (i32.add (i32.const 256) (local.get $off))))
        )
    "#;

    #[test]
    fn config_and_resource_imports_reach_the_plugin() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let rom_path = std::env::temp_dir().join(format!(
            "copperline-wasm-rom-{}-{nanos}.bin",
            std::process::id()
        ));
        std::fs::write(&rom_path, [0xCA, 0xFE, 0xBA, 0xBE]).unwrap();

        let path = write_wasm("cfg", CONFIG_WAT);
        let mut manifest = manifest("cfg", false);
        manifest.config.insert("buffers".into(), "8".into());
        manifest
            .config
            .insert("rom".into(), rom_path.to_string_lossy().into_owned());
        manifest.file_keys = vec!["rom".into()];

        let mut board = WasmBoard::from_file(&path, manifest).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);
        assert_eq!(board.read(0, 1, &mut host), 0x38); // config "buffers" = "8"
        assert_eq!(board.read(1, 1, &mut host), 4); // resource_len("rom")
        assert_eq!(board.read(2, 1, &mut host), 0xCA); // rom[0]

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&rom_path);
    }

    #[test]
    fn net_send_and_recv_round_trip_over_loopback() {
        let path = write_wasm("net", NET_WAT);
        let mut board = WasmBoard::from_file(&path, net_manifest("net")).unwrap();
        let mut mem = empty_memory();
        let mut host = DeviceHost::new(&mut mem);

        // Transmit a frame; the loopback backend queues it straight back.
        board.write(0, 0, 0x5E, &mut host);
        // read() polls it: length 4, first byte 0x5E.
        assert_eq!(board.read(0, 0, &mut host), (4 << 16) | 0x5E);
        // No more frames waiting -> length 0 in the high half.
        assert_eq!(board.read(0, 0, &mut host) >> 16, 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Materialise an inert example plugin `.wasm` (autoconfigures, answers a
    /// constant, no interrupts/DMA) to the path in `COPPERLINE_EMIT_WASM`, for
    /// end-to-end boot testing and as a starting point for plugin authors.
    /// Run with: `COPPERLINE_EMIT_WASM=/path/board.wasm cargo test --release \
    /// emit_example_plugin_wasm -- --ignored`.
    #[test]
    #[ignore]
    fn emit_example_plugin_wasm() {
        let inert = r#"
            (module
              (memory (export "memory") 1)
              (func (export "read") (param i32 i32) (result i32)
                (i32.const 0x12345678))
              (func (export "write") (param i32 i32 i32))
              (func (export "tick") (param i32)))
        "#;
        let out = std::env::var("COPPERLINE_EMIT_WASM").expect("set COPPERLINE_EMIT_WASM");
        std::fs::write(&out, wat::parse_str(inert).expect("valid WAT")).expect("write wasm");
        eprintln!("wrote example plugin to {out}");
    }
}
