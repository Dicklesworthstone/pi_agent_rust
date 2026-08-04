//! PiWasm: WebAssembly polyfill for QuickJS runtime.
//!
//! Provides `globalThis.WebAssembly` inside QuickJS, backed by wasmtime.
//! Enables JS extensions to use WebAssembly modules (e.g., Emscripten-compiled
//! code) even though QuickJS lacks native WebAssembly support.
//!
//! Architecture:
//! - Native Rust functions (`__pi_wasm_*`) handle compile/instantiate/call
//! - A JS polyfill wraps them into the standard `WebAssembly` namespace
//! - Optional staged virtual files and a small host import surface support
//!   Emscripten-style modules such as the DOOM overlay fixture

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use anyhow::anyhow;
use rquickjs::function::Func;
use rquickjs::{ArrayBuffer, Ctx, Value};
use serde::Serialize;
use tracing::debug;
use wasmtime::{
    Caller, Engine, ExternType, Instance as WasmInstance, Linker, Module as WasmModule, Store,
    ToWasmtimeResult as _, Val, ValType,
};

// ---------------------------------------------------------------------------
// Bridge state
// ---------------------------------------------------------------------------

/// Host data stored in each wasmtime `Store`.
struct WasmHostData {
    /// Maximum memory pages allowed (enforced on grow).
    max_memory_pages: u64,
    /// Files staged from JS for modules that expect a host filesystem.
    staged_files: HashMap<String, std::sync::Arc<Vec<u8>>>,
    /// Open virtual file descriptors for staged files.
    open_files: HashMap<u32, VirtualFileHandle>,
    /// Next synthetic file descriptor.
    next_fd: u32,
    /// Monotonic start time for `emscripten_get_now`.
    started_at: Instant,
}

/// Per-instance state: the wasmtime `Store` owns all WASM objects.
struct InstanceState {
    store: Store<WasmHostData>,
    instance: WasmInstance,
}

#[derive(Clone)]
struct VirtualFileHandle {
    path: String,
    position: usize,
    readable: bool,
    writable: bool,
    append: bool,
}

#[derive(Serialize)]
struct WasmExportEntry {
    name: String,
    kind: &'static str,
}

/// Per-JS-runtime WASM bridge state, shared via `Rc<RefCell<>>`.
pub(crate) struct WasmBridgeState {
    engine: Engine,
    modules: HashMap<u32, WasmModule>,
    instances: HashMap<u32, InstanceState>,
    staged_files: HashMap<String, std::sync::Arc<Vec<u8>>>,
    next_id: u32,
    max_modules: usize,
    max_instances: usize,
}

impl WasmBridgeState {
    pub fn new() -> Self {
        let engine = Engine::default();
        Self {
            engine,
            modules: HashMap::new(),
            instances: HashMap::new(),
            staged_files: HashMap::new(),
            next_id: 1,
            max_modules: DEFAULT_MAX_MODULES,
            max_instances: DEFAULT_MAX_INSTANCES,
        }
    }

    fn alloc_id(&mut self) -> Result<u32, String> {
        let start = match self.next_id {
            0 => 1,
            id if id > MAX_JS_WASM_ID => 1,
            id => id,
        };
        let mut candidate = start;

        loop {
            if !self.modules.contains_key(&candidate) && !self.instances.contains_key(&candidate) {
                self.next_id = candidate.wrapping_add(1);
                if self.next_id == 0 || self.next_id > MAX_JS_WASM_ID {
                    self.next_id = 1;
                }
                return Ok(candidate);
            }

            candidate = candidate.wrapping_add(1);
            if candidate == 0 || candidate > MAX_JS_WASM_ID {
                candidate = 1;
            }
            if candidate == start {
                return Err("WASM instance/module id space exhausted".to_string());
            }
        }
    }

    #[cfg(test)]
    fn set_limits_for_test(&mut self, max_modules: usize, max_instances: usize) {
        self.max_modules = max_modules.max(1);
        self.max_instances = max_instances.max(1);
    }
}

const ERRNO_BADF: i32 = 8;
const ERRNO_EXIST: i32 = 20;
const ERRNO_FBIG: i32 = 27;
const ERRNO_INVAL: i32 = 28;
const ERRNO_NOENT: i32 = 44;

const O_ACCMODE: i32 = 0o3;
const O_WRONLY: i32 = 0o1;
const O_RDWR: i32 = 0o2;
const O_CREAT: i32 = 0o100;
const O_EXCL: i32 = 0o200;
const O_TRUNC: i32 = 0o1000;
const O_APPEND: i32 = 0o2000;

/// Hard cap on per-instance virtual file growth to avoid unbounded host allocation.
const MAX_VIRTUAL_FILE_BYTES: usize = 64 * 1024 * 1024;

const fn descriptor_access(flags: i32) -> Option<(bool, bool)> {
    match flags & O_ACCMODE {
        0 => Some((true, false)),
        O_WRONLY => Some((false, true)),
        O_RDWR => Some((true, true)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn throw_wasm(ctx: &Ctx<'_>, class: &str, msg: &str) -> rquickjs::Error {
    let text = format!("{class}: {msg}");
    if let Ok(js_text) = rquickjs::String::from_str(ctx.clone(), &text) {
        let _ = ctx.throw(js_text.into_value());
    }
    rquickjs::Error::Exception
}

fn wasmtime_host_result<T>(operation: impl FnOnce() -> anyhow::Result<T>) -> wasmtime::Result<T> {
    operation().to_wasmtime_result()
}

// ---------------------------------------------------------------------------
// Value conversion: JS ↔ WASM
// ---------------------------------------------------------------------------

fn extract_bytes(ctx: &Ctx<'_>, value: &Value<'_>) -> rquickjs::Result<Vec<u8>> {
    // Try ArrayBuffer
    if let Some(obj) = value.as_object() {
        if let Some(ab) = obj.as_array_buffer() {
            return ab
                .as_bytes()
                .map(<[u8]>::to_vec)
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Detached ArrayBuffer"));
        }
        if let Some(typed) = obj.as_typed_array::<u8>() {
            return typed
                .as_bytes()
                .map(<[u8]>::to_vec)
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Detached TypedArray"));
        }
    }
    // Try array of numbers
    if let Some(arr) = value.as_array() {
        let mut bytes = Vec::with_capacity(arr.len().min(1024 * 1024));
        for i in 0..arr.len() {
            let v: i32 = arr.get(i)?;
            bytes.push(
                u8::try_from(v)
                    .map_err(|_| throw_wasm(ctx, "TypeError", "Byte value out of range"))?,
            );
        }
        return Ok(bytes);
    }
    Err(throw_wasm(
        ctx,
        "TypeError",
        "Expected ArrayBuffer or byte array",
    ))
}

/// Convert a WASM `Val` to an f64 for returning to JS.
/// Note: i64 is intentionally excluded to avoid silent precision loss.
#[allow(clippy::cast_precision_loss)]
fn val_to_f64(ctx: &Ctx<'_>, val: &Val) -> rquickjs::Result<f64> {
    match val {
        Val::I32(v) => Ok(f64::from(*v)),
        Val::F32(bits) => Ok(f64::from(f32::from_bits(*bits))),
        Val::F64(bits) => Ok(f64::from_bits(*bits)),
        _ => Err(throw_wasm(
            ctx,
            "RuntimeError",
            "Unsupported WASM return value type for PiJS bridge",
        )),
    }
}

/// Emulate JavaScript `ToInt32` semantics for number -> i32 coercion.
#[allow(clippy::cast_possible_truncation)]
fn js_to_i32(value: f64) -> i32 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }

    let mut wrapped = value.trunc() % TWO_POW_32;
    if wrapped < 0.0 {
        wrapped += TWO_POW_32;
    }

    if wrapped >= TWO_POW_31 {
        (wrapped - TWO_POW_32) as i32
    } else {
        wrapped as i32
    }
}

#[allow(clippy::cast_possible_truncation)]
fn js_to_val(ctx: &Ctx<'_>, value: &Value<'_>, ty: &ValType) -> rquickjs::Result<Val> {
    match ty {
        ValType::I32 => {
            let v: f64 = value
                .as_number()
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Expected number for i32"))?;
            Ok(Val::I32(js_to_i32(v)))
        }
        ValType::I64 => Err(throw_wasm(
            ctx,
            "TypeError",
            "i64 parameters are not supported by PiJS WebAssembly bridge",
        )),
        ValType::F32 => {
            let v: f64 = value
                .as_number()
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Expected number for f32"))?;
            #[expect(clippy::cast_possible_truncation)]
            Ok(Val::F32((v as f32).to_bits()))
        }
        ValType::F64 => {
            let v: f64 = value
                .as_number()
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Expected number for f64"))?;
            Ok(Val::F64(v.to_bits()))
        }
        _ => Err(throw_wasm(ctx, "TypeError", "Unsupported WASM value type")),
    }
}

fn validate_call_result_types(ctx: &Ctx<'_>, result_types: &[ValType]) -> rquickjs::Result<()> {
    if result_types.len() > 1 {
        return Err(throw_wasm(
            ctx,
            "RuntimeError",
            "Multi-value WASM results are not supported by PiJS WebAssembly bridge",
        ));
    }

    if let Some(ty) = result_types.first() {
        return match ty {
            ValType::I32 | ValType::F32 | ValType::F64 => Ok(()),
            ValType::I64 => Err(throw_wasm(
                ctx,
                "RuntimeError",
                "i64 results are not supported by PiJS WebAssembly bridge",
            )),
            _ => Err(throw_wasm(
                ctx,
                "RuntimeError",
                "Unsupported WASM return type for PiJS WebAssembly bridge",
            )),
        };
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Import helpers
// ---------------------------------------------------------------------------

fn instance_memory(inst: &mut InstanceState, mem_name: &str) -> anyhow::Result<wasmtime::Memory> {
    inst.instance
        .get_memory(&mut inst.store, mem_name)
        .ok_or_else(|| anyhow!("Memory '{mem_name}' not found"))
}

fn caller_memory(caller: &mut Caller<'_, WasmHostData>) -> anyhow::Result<wasmtime::Memory> {
    caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| anyhow!("Exported memory 'memory' not found"))
}

fn checked_memory_range(
    offset: usize,
    len: usize,
    memory_len: usize,
) -> anyhow::Result<std::ops::Range<usize>> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| anyhow!("Memory access overflow"))?;
    if end > memory_len {
        return Err(anyhow!("Memory access out of bounds"));
    }
    Ok(offset..end)
}

fn caller_read_bytes(
    caller: &mut Caller<'_, WasmHostData>,
    offset: usize,
    len: usize,
) -> anyhow::Result<Vec<u8>> {
    let memory = caller_memory(caller)?;
    let _ = checked_memory_range(offset, len, memory.data_size(&mut *caller))?;
    let mut bytes = vec![0_u8; len];
    memory
        .read(&mut *caller, offset, &mut bytes)
        .map_err(anyhow::Error::from)?;
    Ok(bytes)
}

fn caller_write_bytes(
    caller: &mut Caller<'_, WasmHostData>,
    offset: usize,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let memory = caller_memory(caller)?;
    let _ = checked_memory_range(offset, bytes.len(), memory.data_size(&mut *caller))?;
    memory
        .write(&mut *caller, offset, bytes)
        .map_err(anyhow::Error::from)
}

fn caller_read_u32(caller: &mut Caller<'_, WasmHostData>, offset: usize) -> anyhow::Result<u32> {
    let bytes = caller_read_bytes(caller, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn caller_write_u32(
    caller: &mut Caller<'_, WasmHostData>,
    offset: usize,
    value: u32,
) -> anyhow::Result<()> {
    caller_write_bytes(caller, offset, &value.to_le_bytes())
}

fn caller_write_u64(
    caller: &mut Caller<'_, WasmHostData>,
    offset: usize,
    value: u64,
) -> anyhow::Result<()> {
    caller_write_bytes(caller, offset, &value.to_le_bytes())
}

fn caller_read_c_string(
    caller: &mut Caller<'_, WasmHostData>,
    offset: usize,
) -> anyhow::Result<String> {
    let memory = caller_memory(caller)?;
    let bytes = memory.data(&mut *caller);
    let mut end = offset;
    while end < bytes.len() && bytes[end] != 0 {
        end += 1;
    }
    if end >= bytes.len() {
        return Err(anyhow!("Unterminated string in WASM memory"));
    }
    Ok(String::from_utf8_lossy(&bytes[offset..end]).into_owned())
}

fn val_i32(params: &[Val], idx: usize, label: &str) -> anyhow::Result<i32> {
    match params.get(idx) {
        Some(Val::I32(value)) => Ok(*value),
        _ => Err(anyhow!("Expected i32 parameter '{label}' at index {idx}")),
    }
}

fn val_i64(params: &[Val], idx: usize, label: &str) -> anyhow::Result<i64> {
    match params.get(idx) {
        Some(Val::I64(value)) => Ok(*value),
        Some(Val::I32(value)) => Ok(i64::from(*value)),
        _ => Err(anyhow!("Expected i64 parameter '{label}' at index {idx}")),
    }
}

const fn set_i32_result(results: &mut [Val], value: i32) {
    if !results.is_empty() {
        results[0] = Val::I32(value);
    }
}

const fn set_f64_result(results: &mut [Val], value: f64) {
    if !results.is_empty() {
        results[0] = Val::F64(value.to_bits());
    }
}

const SUPPORTED_HOST_IMPORTS: &[&str] = &[
    "__syscall_openat",
    "fd_close",
    "fd_read",
    "fd_seek",
    "fd_write",
    "emscripten_get_now",
    "emscripten_resize_heap",
];

const COMPAT_STUB_IMPORTS: &[&str] = &[
    "__syscall_fcntl64",
    "__syscall_ioctl",
    "__syscall_mkdirat",
    "__syscall_renameat",
    "__syscall_rmdir",
    "__syscall_unlinkat",
    "_emscripten_system",
    "exit",
];

fn unsupported_import_error(mod_name: &str, imp_name: &str) -> String {
    format!(
        "Unsupported WASM import {mod_name}.{imp_name}; PiWasm fails closed instead of creating ambient default stubs. Supported host imports: {}; compatibility stubs: {}",
        SUPPORTED_HOST_IMPORTS.join(", "),
        COMPAT_STUB_IMPORTS.join(", ")
    )
}

fn register_compat_stub_import(
    linker: &mut Linker<WasmHostData>,
    mod_name: &str,
    imp_name: &str,
    func_ty: &wasmtime::FuncType,
) -> Result<(), String> {
    let result_types: Vec<ValType> = func_ty.results().collect();
    let import_label = format!("{mod_name}.{imp_name}");
    if result_types
        .iter()
        .any(|ty| Val::default_for_ty(ty).is_none())
    {
        return Err(format!(
            "Compatibility stub {import_label} has unsupported default result type"
        ));
    }
    linker
        .func_new(
            mod_name,
            imp_name,
            func_ty.clone(),
            move |_caller: Caller<'_, WasmHostData>, _params: &[Val], results: &mut [Val]| {
                wasmtime_host_result(|| {
                    for (i, ty) in result_types.iter().enumerate() {
                        results[i] = Val::default_for_ty(ty).ok_or_else(|| {
                            anyhow!("Unsupported default result for {import_label}")
                        })?;
                    }
                    Ok(())
                })
            },
        )
        .map_err(|e| format!("Failed to register compatibility stub {mod_name}.{imp_name}: {e}"))?;
    Ok(())
}

/// Register the small host import surface PiWasm currently supports.
/// Unsupported imports fail closed. The only default-return compatibility stubs
/// are the explicit Emscripten names in `COMPAT_STUB_IMPORTS`.
#[allow(clippy::too_many_lines)]
fn register_host_imports(
    linker: &mut Linker<WasmHostData>,
    module: &WasmModule,
) -> Result<(), String> {
    for import in module.imports() {
        let mod_name = import.module();
        let imp_name = import.name();
        if let ExternType::Func(func_ty) = import.ty() {
            match imp_name {
                "__syscall_openat" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |mut caller, params, results| {
                                wasmtime_host_result(|| {
                                    let path_ptr = usize::try_from(val_i32(params, 1, "path")?)
                                        .map_err(|_| anyhow!("Negative path pointer"))?;
                                    let flags = val_i32(params, 2, "flags")?;
                                    let path = caller_read_c_string(&mut caller, path_ptr)?;
                                    let Some((readable, writable)) = descriptor_access(flags)
                                    else {
                                        set_i32_result(results, -ERRNO_INVAL);
                                        return Ok(());
                                    };
                                    if flags & O_TRUNC != 0 && !writable {
                                        set_i32_result(results, -ERRNO_INVAL);
                                        return Ok(());
                                    }

                                    let append = flags & O_APPEND != 0;
                                    let (fd, bytes_len) = {
                                        let host = caller.data_mut();
                                        let path_exists = host.staged_files.contains_key(&path);
                                        if !path_exists {
                                            if flags & O_CREAT == 0 {
                                                set_i32_result(results, -ERRNO_NOENT);
                                                return Ok(());
                                            }
                                            host.staged_files.insert(
                                                path.clone(),
                                                std::sync::Arc::new(Vec::new()),
                                            );
                                        } else if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                                            set_i32_result(results, -ERRNO_EXIST);
                                            return Ok(());
                                        }

                                        let (position, bytes_len) = {
                                            let file_arc = host
                                                .staged_files
                                                .get_mut(&path)
                                                .ok_or_else(|| {
                                                    anyhow!("Virtual file disappeared during open")
                                                })?;
                                            if flags & O_TRUNC != 0 {
                                                std::sync::Arc::make_mut(file_arc).clear();
                                            }
                                            let bytes_len = file_arc.len();
                                            let position = if append { bytes_len } else { 0 };
                                            (position, bytes_len)
                                        };

                                        if host.next_fd == u32::MAX {
                                            return Err(anyhow!("Synthetic fd space exhausted"));
                                        }
                                        let fd = host.next_fd;
                                        host.next_fd = host.next_fd.saturating_add(1);
                                        host.open_files.insert(
                                            fd,
                                            VirtualFileHandle {
                                                path: path.clone(),
                                                position,
                                                readable,
                                                writable,
                                                append,
                                            },
                                        );
                                        (fd, bytes_len)
                                    };
                                    debug!(
                                        path,
                                        bytes = bytes_len,
                                        fd,
                                        readable,
                                        writable,
                                        append,
                                        "wasm: staged file open"
                                    );
                                    set_i32_result(results, i32::try_from(fd).unwrap_or(i32::MAX));
                                    Ok(())
                                })
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                "fd_read" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |mut caller, params, results| {
                                wasmtime_host_result(|| {
                                    let fd = u32::try_from(val_i32(params, 0, "fd")?)
                                        .map_err(|_| anyhow!("Negative fd"))?;
                                    let iov = usize::try_from(val_i32(params, 1, "iov")?)
                                        .map_err(|_| anyhow!("Negative iov pointer"))?;
                                    let iovcnt = usize::try_from(val_i32(params, 2, "iovcnt")?)
                                        .map_err(|_| anyhow!("Negative iov count"))?;
                                    let pnum = usize::try_from(val_i32(params, 3, "pnum")?)
                                        .map_err(|_| anyhow!("Negative pnum pointer"))?;

                                    let (path, mut position) =
                                        if let Some(handle) = caller.data().open_files.get(&fd) {
                                            if !handle.readable {
                                                set_i32_result(results, ERRNO_BADF);
                                                return Ok(());
                                            }
                                            (handle.path.clone(), handle.position)
                                        } else {
                                            set_i32_result(results, ERRNO_BADF);
                                            return Ok(());
                                        };

                                    let mut total = 0_usize;
                                    for index in 0..iovcnt {
                                        let base = iov
                                            .checked_add(index.saturating_mul(8))
                                            .ok_or_else(|| anyhow!("iov overflow"))?;
                                        let ptr =
                                            usize::try_from(caller_read_u32(&mut caller, base)?)
                                                .map_err(|_| anyhow!("iov ptr overflow"))?;
                                        let len = usize::try_from(caller_read_u32(
                                            &mut caller,
                                            base + 4,
                                        )?)
                                        .map_err(|_| anyhow!("iov len overflow"))?;
                                        let chunk = {
                                            let host = caller.data();
                                            let Some(file) = host.staged_files.get(&path) else {
                                                set_i32_result(results, ERRNO_NOENT);
                                                return Ok(());
                                            };
                                            if position >= file.len() || len == 0 {
                                                Vec::new()
                                            } else {
                                                let available = file.len().saturating_sub(position);
                                                let to_copy = available.min(len);
                                                file[position..position + to_copy].to_vec()
                                            }
                                        };
                                        if chunk.is_empty() {
                                            break;
                                        }
                                        caller_write_bytes(&mut caller, ptr, &chunk)?;
                                        position += chunk.len();
                                        total += chunk.len();
                                        if chunk.len() < len {
                                            break;
                                        }
                                    }

                                    caller_write_u32(
                                        &mut caller,
                                        pnum,
                                        u32::try_from(total).unwrap_or(u32::MAX),
                                    )?;
                                    if let Some(handle) = caller.data_mut().open_files.get_mut(&fd)
                                    {
                                        handle.position = position;
                                    } else {
                                        set_i32_result(results, ERRNO_BADF);
                                        return Ok(());
                                    }
                                    set_i32_result(results, 0);
                                    Ok(())
                                })
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                "fd_seek" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |mut caller, params, results| {
                                wasmtime_host_result(|| {
                                    let fd = u32::try_from(val_i32(params, 0, "fd")?)
                                        .map_err(|_| anyhow!("Negative fd"))?;
                                    let offset = val_i64(params, 1, "offset")?;
                                    let whence = val_i32(params, 2, "whence")?;
                                    let new_offset_ptr =
                                        usize::try_from(val_i32(params, 3, "newOffset")?)
                                            .map_err(|_| anyhow!("Negative newOffset pointer"))?;

                                    let (path, current_position) =
                                        if let Some(handle) = caller.data().open_files.get(&fd) {
                                            (handle.path.clone(), handle.position)
                                        } else {
                                            set_i32_result(results, ERRNO_BADF);
                                            return Ok(());
                                        };
                                    let Some(file_len) =
                                        caller.data().staged_files.get(&path).map(|v| v.len())
                                    else {
                                        set_i32_result(results, ERRNO_NOENT);
                                        return Ok(());
                                    };
                                    let base = match whence {
                                        0 => 0_i64,
                                        1 => i64::try_from(current_position).unwrap_or(i64::MAX),
                                        2 => i64::try_from(file_len).unwrap_or(i64::MAX),
                                        _ => {
                                            set_i32_result(results, ERRNO_INVAL);
                                            return Ok(());
                                        }
                                    };
                                    let next = base
                                        .checked_add(offset)
                                        .ok_or_else(|| anyhow!("Seek overflow"))?;
                                    if next < 0 {
                                        set_i32_result(results, ERRNO_INVAL);
                                        return Ok(());
                                    }
                                    let next = usize::try_from(next)
                                        .map_err(|_| anyhow!("Seek overflow"))?;
                                    if let Some(handle) = caller.data_mut().open_files.get_mut(&fd)
                                    {
                                        handle.position = next;
                                    } else {
                                        set_i32_result(results, ERRNO_BADF);
                                        return Ok(());
                                    }
                                    caller_write_u64(
                                        &mut caller,
                                        new_offset_ptr,
                                        u64::try_from(next).unwrap_or(u64::MAX),
                                    )?;
                                    set_i32_result(results, 0);
                                    Ok(())
                                })
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                "fd_close" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |mut caller, params, results| {
                                wasmtime_host_result(|| {
                                    let fd = u32::try_from(val_i32(params, 0, "fd")?)
                                        .map_err(|_| anyhow!("Negative fd"))?;
                                    let result =
                                        if caller.data_mut().open_files.remove(&fd).is_some() {
                                            0
                                        } else {
                                            ERRNO_BADF
                                        };
                                    set_i32_result(results, result);
                                    Ok(())
                                })
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                "fd_write" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |mut caller, params, results| {
                                wasmtime_host_result(|| {
                                    let fd = u32::try_from(val_i32(params, 0, "fd")?)
                                        .map_err(|_| anyhow!("Negative fd"))?;
                                    let iov = usize::try_from(val_i32(params, 1, "iov")?)
                                        .map_err(|_| anyhow!("Negative iov pointer"))?;
                                    let iovcnt = usize::try_from(val_i32(params, 2, "iovcnt")?)
                                        .map_err(|_| anyhow!("Negative iov count"))?;
                                    let pnum = usize::try_from(val_i32(params, 3, "pnum")?)
                                        .map_err(|_| anyhow!("Negative pnum pointer"))?;
                                    let (path, mut position, append, file_len) = {
                                        let host = caller.data();
                                        if let Some(handle) = host.open_files.get(&fd) {
                                            if !handle.writable {
                                                set_i32_result(results, ERRNO_BADF);
                                                return Ok(());
                                            }
                                            let Some(file_len) = host
                                                .staged_files
                                                .get(&handle.path)
                                                .map(|v| v.len())
                                            else {
                                                set_i32_result(results, ERRNO_NOENT);
                                                return Ok(());
                                            };
                                            (
                                                handle.path.clone(),
                                                handle.position,
                                                handle.append,
                                                file_len,
                                            )
                                        } else {
                                            set_i32_result(results, ERRNO_BADF);
                                            return Ok(());
                                        }
                                    };
                                    let base_position = if append { file_len } else { position };
                                    let mut total = 0_usize;
                                    let mut chunks = Vec::new();
                                    for index in 0..iovcnt {
                                        let base = iov
                                            .checked_add(index.saturating_mul(8))
                                            .ok_or_else(|| anyhow!("iov overflow"))?;
                                        let ptr =
                                            usize::try_from(caller_read_u32(&mut caller, base)?)
                                                .map_err(|_| anyhow!("iov ptr overflow"))?;
                                        let len = usize::try_from(caller_read_u32(
                                            &mut caller,
                                            base + 4,
                                        )?)
                                        .map_err(|_| anyhow!("iov len overflow"))?;
                                        if len == 0 {
                                            continue;
                                        }
                                        let next_total =
                                            total.checked_add(len).ok_or_else(|| {
                                                anyhow!("fd_write byte count overflow")
                                            })?;
                                        if base_position
                                            .checked_add(next_total)
                                            .ok_or_else(|| anyhow!("fd_write overflow"))?
                                            > MAX_VIRTUAL_FILE_BYTES
                                        {
                                            set_i32_result(results, ERRNO_FBIG);
                                            return Ok(());
                                        }

                                        let bytes = caller_read_bytes(&mut caller, ptr, len)?;
                                        total = next_total;
                                        chunks.push(bytes);
                                    }
                                    if total == 0 {
                                        caller_write_u32(&mut caller, pnum, 0)?;
                                        if let Some(handle) =
                                            caller.data_mut().open_files.get_mut(&fd)
                                        {
                                            handle.position = position;
                                        } else {
                                            set_i32_result(results, ERRNO_BADF);
                                            return Ok(());
                                        }
                                        set_i32_result(results, 0);
                                        return Ok(());
                                    }
                                    {
                                        let host = caller.data_mut();
                                        let Some(file_arc) = host.staged_files.get_mut(&path)
                                        else {
                                            set_i32_result(results, ERRNO_NOENT);
                                            return Ok(());
                                        };
                                        let file = std::sync::Arc::make_mut(file_arc);
                                        if append {
                                            position = base_position;
                                        }
                                        let end = position
                                            .checked_add(total)
                                            .ok_or_else(|| anyhow!("fd_write overflow"))?;
                                        if end > MAX_VIRTUAL_FILE_BYTES {
                                            set_i32_result(results, ERRNO_FBIG);
                                            return Ok(());
                                        }
                                        if position > file.len() {
                                            file.resize(position, 0);
                                        }
                                        if end > file.len() {
                                            file.resize(end, 0);
                                        }

                                        // Stage guest buffers before mutating the virtual file so
                                        // size-limit failures do not commit a partial multi-iov write.
                                        let mut write_position = position;
                                        for bytes in &chunks {
                                            let chunk_end = write_position
                                                .checked_add(bytes.len())
                                                .ok_or_else(|| anyhow!("fd_write overflow"))?;
                                            file[write_position..chunk_end].copy_from_slice(bytes);
                                            write_position = chunk_end;
                                        }
                                        position = write_position;
                                    }
                                    caller_write_u32(
                                        &mut caller,
                                        pnum,
                                        u32::try_from(total).unwrap_or(u32::MAX),
                                    )?;
                                    if let Some(handle) = caller.data_mut().open_files.get_mut(&fd)
                                    {
                                        handle.position = position;
                                    } else {
                                        set_i32_result(results, ERRNO_BADF);
                                        return Ok(());
                                    }
                                    set_i32_result(results, 0);
                                    Ok(())
                                })
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                "emscripten_get_now" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |caller, _params, results| {
                                let elapsed_ms =
                                    caller.data().started_at.elapsed().as_secs_f64() * 1000.0;
                                set_f64_result(results, elapsed_ms);
                                Ok(())
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                "emscripten_resize_heap" => {
                    linker
                        .func_new(
                            mod_name,
                            imp_name,
                            func_ty.clone(),
                            move |mut caller, params, results| {
                                wasmtime_host_result(|| {
                                    let requested_size =
                                        usize::try_from(val_i32(params, 0, "requestedSize")?)
                                            .map_err(|_| anyhow!("Negative heap size"))?;
                                    let memory = caller_memory(&mut caller)?;
                                    let current_size = memory.data_size(&mut caller);
                                    if requested_size <= current_size {
                                        set_i32_result(results, 1);
                                        return Ok(());
                                    }
                                    let page_size = usize::try_from(memory.page_size(&caller))
                                        .unwrap_or(65_536);
                                    let needed_bytes = requested_size.saturating_sub(current_size);
                                    let needed_pages =
                                        (needed_bytes.saturating_add(page_size - 1)) / page_size;
                                    let current_pages = memory.size(&caller);
                                    let requested_pages = current_pages.saturating_add(
                                        u64::try_from(needed_pages).unwrap_or(u64::MAX),
                                    );
                                    if requested_pages > caller.data().max_memory_pages {
                                        set_i32_result(results, 0);
                                        return Ok(());
                                    }
                                    let grown = memory
                                        .grow(
                                            &mut caller,
                                            u64::try_from(needed_pages).unwrap_or(u64::MAX),
                                        )
                                        .is_ok();
                                    set_i32_result(results, i32::from(grown));
                                    Ok(())
                                })
                            },
                        )
                        .map_err(|e| {
                            format!("Failed to register import {mod_name}.{imp_name}: {e}")
                        })?;
                }
                name if COMPAT_STUB_IMPORTS.contains(&name) => {
                    register_compat_stub_import(linker, mod_name, imp_name, &func_ty)?;
                }
                _ => return Err(unsupported_import_error(mod_name, imp_name)),
            }
        } else {
            return Err(unsupported_import_error(mod_name, imp_name));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API: inject globalThis.WebAssembly
// ---------------------------------------------------------------------------

/// Maximum default memory pages (64 KiB per page → 64 MB).
const DEFAULT_MAX_MEMORY_PAGES: u64 = 1024;
/// Hard limit on compiled modules kept alive in one JS runtime.
const DEFAULT_MAX_MODULES: usize = 256;
/// Hard limit on instantiated modules kept alive in one JS runtime.
const DEFAULT_MAX_INSTANCES: usize = 256;
/// Keep IDs within QuickJS signed-int range for stable JS↔Rust roundtrips.
const MAX_JS_WASM_ID: u32 = i32::MAX as u32;
/// JS numeric coercion helpers.
const TWO_POW_32: f64 = 4_294_967_296.0;
const TWO_POW_31: f64 = 2_147_483_648.0;

/// Inject `globalThis.WebAssembly` polyfill into the QuickJS context.
#[allow(clippy::too_many_lines)]
pub(crate) fn inject_wasm_globals(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<WasmBridgeState>>,
) -> rquickjs::Result<()> {
    let global = ctx.globals();

    // ---- __pi_wasm_compile_native(bytes) → module_id ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_compile_native",
            Func::from(
                move |ctx: Ctx<'_>, bytes_val: Value<'_>| -> rquickjs::Result<u32> {
                    let bytes = extract_bytes(&ctx, &bytes_val)?;
                    if bytes.len() > MAX_VIRTUAL_FILE_BYTES {
                        return Err(throw_wasm(
                            &ctx,
                            "CompileError",
                            &format!(
                                "Module exceeds PiWasm limit ({} > {} bytes)",
                                bytes.len(),
                                MAX_VIRTUAL_FILE_BYTES
                            ),
                        ));
                    }
                    let mut bridge = st.borrow_mut();
                    if bridge.modules.len() >= bridge.max_modules {
                        return Err(throw_wasm(
                            &ctx,
                            "CompileError",
                            &format!("Module limit reached ({})", bridge.max_modules),
                        ));
                    }
                    let module = WasmModule::from_binary(&bridge.engine, &bytes)
                        .map_err(|e| throw_wasm(&ctx, "CompileError", &e.to_string()))?;
                    let id = bridge
                        .alloc_id()
                        .map_err(|e| throw_wasm(&ctx, "CompileError", &e))?;
                    debug!(module_id = id, bytes_len = bytes.len(), "wasm: compiled");
                    bridge.modules.insert(id, module);
                    Ok(id)
                },
            ),
        )?;
    }

    // ---- __pi_wasm_validate_native(bytes) → bool ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_validate_native",
            Func::from(
                move |ctx: Ctx<'_>, bytes_val: Value<'_>| -> rquickjs::Result<bool> {
                    let bytes = extract_bytes(&ctx, &bytes_val)?;
                    if bytes.len() > MAX_VIRTUAL_FILE_BYTES {
                        return Ok(false);
                    }
                    let bridge = st.borrow();
                    Ok(WasmModule::from_binary(&bridge.engine, &bytes).is_ok())
                },
            ),
        )?;
    }

    // ---- __pi_wasm_stage_file_native(path, bytes) → byte_length ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_stage_file_native",
            Func::from(
                move |ctx: Ctx<'_>, path: String, bytes_val: Value<'_>| -> rquickjs::Result<u32> {
                    let bytes = extract_bytes(&ctx, &bytes_val)?;
                    if bytes.len() > MAX_VIRTUAL_FILE_BYTES {
                        return Err(throw_wasm(
                            &ctx,
                            "RangeError",
                            &format!(
                                "Virtual file exceeds PiWasm limit ({} > {} bytes)",
                                bytes.len(),
                                MAX_VIRTUAL_FILE_BYTES
                            ),
                        ));
                    }
                    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                    debug!(path = %path, len_bytes = bytes.len(), "wasm: staged file");
                    st.borrow_mut()
                        .staged_files
                        .insert(path, std::sync::Arc::new(bytes));
                    Ok(len)
                },
            ),
        )?;
    }

    // ---- __pi_wasm_instantiate_native(module_id) → instance_id ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_instantiate_native",
            Func::from(
                move |ctx: Ctx<'_>, module_id: u32| -> rquickjs::Result<u32> {
                    let mut bridge = st.borrow_mut();
                    if bridge.instances.len() >= bridge.max_instances {
                        return Err(throw_wasm(
                            &ctx,
                            "RuntimeError",
                            &format!("Instance limit reached ({})", bridge.max_instances),
                        ));
                    }
                    let module = bridge
                        .modules
                        .get(&module_id)
                        .ok_or_else(|| throw_wasm(&ctx, "LinkError", "Module not found"))?
                        .clone();

                    let mut linker = Linker::new(&bridge.engine);
                    register_host_imports(&mut linker, &module)
                        .map_err(|e| throw_wasm(&ctx, "LinkError", &e))?;

                    let mut store = Store::new(
                        &bridge.engine,
                        WasmHostData {
                            max_memory_pages: DEFAULT_MAX_MEMORY_PAGES,
                            staged_files: bridge.staged_files.clone(),
                            open_files: HashMap::new(),
                            next_fd: 3,
                            started_at: Instant::now(),
                        },
                    );
                    let instance = linker
                        .instantiate(&mut store, &module)
                        .map_err(|e| throw_wasm(&ctx, "LinkError", &e.to_string()))?;

                    let id = bridge
                        .alloc_id()
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e))?;
                    debug!(instance_id = id, module_id, "wasm: instantiated");
                    bridge
                        .instances
                        .insert(id, InstanceState { store, instance });
                    Ok(id)
                },
            ),
        )?;
    }

    // ---- __pi_wasm_get_exports_native(instance_id) → JSON string [{name, kind}] ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_get_exports_native",
            Func::from(
                move |ctx: Ctx<'_>, instance_id: u32| -> rquickjs::Result<String> {
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;

                    let mut entries: Vec<WasmExportEntry> = Vec::new();
                    for export in inst.instance.exports(&mut inst.store) {
                        let name = export.name().to_string();
                        let kind = match export.into_extern() {
                            wasmtime::Extern::Func(_) => "func",
                            wasmtime::Extern::Memory(_) => "memory",
                            wasmtime::Extern::Table(_) => "table",
                            wasmtime::Extern::Global(_) => "global",
                            wasmtime::Extern::SharedMemory(_) => "shared-memory",
                            wasmtime::Extern::Tag(_) => "tag",
                        };
                        entries.push(WasmExportEntry { name, kind });
                    }
                    serde_json::to_string(&entries)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))
                },
            ),
        )?;
    }

    // ---- __pi_wasm_call_export_native(instance_id, name, args_array) → f64 result ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_call_export_native",
            Func::from(
                move |ctx: Ctx<'_>,
                      instance_id: u32,
                      name: String,
                      args_val: Value<'_>|
                      -> rquickjs::Result<f64> {
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;

                    let started = Instant::now();
                    let func = inst
                        .instance
                        .get_func(&mut inst.store, &name)
                        .ok_or_else(|| {
                            throw_wasm(&ctx, "RuntimeError", &format!("Export '{name}' not found"))
                        })?;

                    let func_ty = func.ty(&inst.store);
                    let param_types: Vec<ValType> = func_ty.params().collect();
                    if param_types.iter().any(|ty| matches!(ty, ValType::I64)) {
                        return Err(throw_wasm(
                            &ctx,
                            "TypeError",
                            "i64 parameters are not supported by PiJS WebAssembly bridge",
                        ));
                    }

                    // Convert JS args to WASM vals
                    let args_arr = args_val
                        .as_array()
                        .ok_or_else(|| throw_wasm(&ctx, "TypeError", "args must be an array"))?;
                    let mut params = Vec::with_capacity(param_types.len());
                    for (i, ty) in param_types.iter().enumerate() {
                        let js_val: Value<'_> = args_arr.get(i)?;
                        params.push(js_to_val(&ctx, &js_val, ty)?);
                    }

                    // Allocate results
                    let result_types: Vec<ValType> = func_ty.results().collect();
                    validate_call_result_types(&ctx, &result_types)?;
                    let mut results: Vec<Val> = result_types
                        .iter()
                        .map(|ty| Val::default_for_ty(ty).unwrap_or(Val::I32(0)))
                        .collect();

                    debug!(
                        instance_id,
                        export = %name,
                        argc = params.len(),
                        "wasm: call export start"
                    );
                    func.call(&mut inst.store, &params, &mut results)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    debug!(
                        instance_id,
                        export = %name,
                        argc = params.len(),
                        elapsed_ms = started.elapsed().as_millis(),
                        "wasm: call export"
                    );

                    // Return first result as f64 (supports i32/f32/f64 only).
                    results.first().map_or(Ok(0.0), |val| val_to_f64(&ctx, val))
                },
            ),
        )?;
    }

    // ---- __pi_wasm_memory_read_native(instance_id, mem_name, offset, len) → byte array ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_memory_read_native",
            Func::from(
                move |ctx: Ctx<'_>,
                      instance_id: u32,
                      mem_name: String,
                      offset: u32,
                      len: u32|
                      -> rquickjs::Result<Vec<u8>> {
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;
                    let memory = instance_memory(inst, &mem_name)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    let start = usize::try_from(offset)
                        .map_err(|_| throw_wasm(&ctx, "RuntimeError", "Offset overflow"))?;
                    let len = usize::try_from(len)
                        .map_err(|_| throw_wasm(&ctx, "RuntimeError", "Length overflow"))?;
                    let data = memory.data(&inst.store);
                    let range = checked_memory_range(start, len, data.len())
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    Ok(data[range].to_vec())
                },
            ),
        )?;
    }

    // ---- __pi_wasm_memory_write_native(instance_id, mem_name, offset, bytes) → byte_length ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_memory_write_native",
            Func::from(
                move |ctx: Ctx<'_>,
                      instance_id: u32,
                      mem_name: String,
                      offset: u32,
                      bytes_val: Value<'_>|
                      -> rquickjs::Result<u32> {
                    let bytes = extract_bytes(&ctx, &bytes_val)?;
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;
                    let memory = instance_memory(inst, &mem_name)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    let start = usize::try_from(offset)
                        .map_err(|_| throw_wasm(&ctx, "RuntimeError", "Offset overflow"))?;
                    let _ =
                        checked_memory_range(start, bytes.len(), memory.data_size(&mut inst.store))
                            .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    memory
                        .write(&mut inst.store, start, &bytes)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    Ok(u32::try_from(bytes.len()).unwrap_or(u32::MAX))
                },
            ),
        )?;
    }

    // ---- __pi_wasm_get_buffer_native(instance_id, mem_name) → stores ArrayBuffer in global ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_get_buffer_native",
            Func::from(
                move |ctx: Ctx<'_>, instance_id: u32, mem_name: String| -> rquickjs::Result<i32> {
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;
                    let started = Instant::now();
                    let memory = instance_memory(inst, &mem_name)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;
                    let data = memory.data(&inst.store);
                    let len = i32::try_from(data.len()).unwrap_or(i32::MAX);
                    let buffer = ArrayBuffer::new_copy(ctx.clone(), data)?;
                    ctx.globals().set("__pi_wasm_tmp_buf", buffer)?;
                    debug!(
                        instance_id,
                        memory = %mem_name,
                        len_bytes = data.len(),
                        elapsed_ms = started.elapsed().as_millis(),
                        "wasm: get memory buffer"
                    );
                    Ok(len)
                },
            ),
        )?;
    }

    // ---- __pi_wasm_memory_grow_native(instance_id, mem_name, delta) → prev_pages ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_memory_grow_native",
            Func::from(
                move |ctx: Ctx<'_>,
                      instance_id: u32,
                      mem_name: String,
                      delta: u32|
                      -> rquickjs::Result<i32> {
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;

                    // Enforce policy limit
                    let memory = inst
                        .instance
                        .get_memory(&mut inst.store, &mem_name)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Memory not found"))?;
                    let current = memory.size(&inst.store);
                    let requested = current.saturating_add(u64::from(delta));
                    if requested > inst.store.data().max_memory_pages {
                        return Ok(-1); // growth denied by policy
                    }

                    Ok(memory
                        .grow(&mut inst.store, u64::from(delta))
                        .map_or(-1, |prev| i32::try_from(prev).unwrap_or(-1)))
                },
            ),
        )?;
    }

    // ---- __pi_wasm_memory_size_native(instance_id, mem_name) → pages ----
    {
        let st = Rc::clone(state);
        global.set(
            "__pi_wasm_memory_size_native",
            Func::from(
                move |ctx: Ctx<'_>, instance_id: u32, mem_name: String| -> rquickjs::Result<u32> {
                    let mut bridge = st.borrow_mut();
                    let inst = bridge
                        .instances
                        .get_mut(&instance_id)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Instance not found"))?;
                    let memory = inst
                        .instance
                        .get_memory(&mut inst.store, &mem_name)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Memory not found"))?;
                    Ok(u32::try_from(memory.size(&inst.store)).unwrap_or(u32::MAX))
                },
            ),
        )?;
    }

    // ---- Inject the JS polyfill layer ----
    ctx.eval::<(), _>(WASM_POLYFILL_JS)?;

    debug!("wasm: globalThis.WebAssembly polyfill injected");
    Ok(())
}

// ---------------------------------------------------------------------------
// JS polyfill that wraps the native functions
// ---------------------------------------------------------------------------

const WASM_POLYFILL_JS: &str = r#"
(function() {
  "use strict";

  class CompileError extends Error {
    constructor(msg) { super(msg); this.name = "CompileError"; }
  }
  class LinkError extends Error {
    constructor(msg) { super(msg); this.name = "LinkError"; }
  }
  class RuntimeError extends Error {
    constructor(msg) { super(msg); this.name = "RuntimeError"; }
  }

  // Synchronous thenable: behaves like syncResolve() but executes
  // .then() callbacks immediately. QuickJS doesn't auto-flush microtasks.
  function syncResolve(value) {
    return {
      then: function(resolve, _reject) {
        try {
          var r = resolve(value);
          return syncResolve(r);
        } catch(e) { return syncReject(e); }
      },
      "catch": function() { return syncResolve(value); }
    };
  }
  function syncReject(err) {
    return {
      then: function(_resolve, reject) {
        if (reject) { reject(err); return syncResolve(undefined); }
        return syncReject(err);
      },
      "catch": function(fn) { fn(err); return syncResolve(undefined); }
    };
  }

  function normalizeBytes(source) {
    if (source instanceof ArrayBuffer) {
      return new Uint8Array(source);
    }
    if (ArrayBuffer.isView && ArrayBuffer.isView(source)) {
      return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
    }
    if (Array.isArray(source)) {
      return new Uint8Array(source);
    }
    throw new CompileError("Invalid source: expected ArrayBuffer, TypedArray, or byte array");
  }

  function resolveStreamingBytes(source) {
    if (source && typeof source.arrayBuffer === "function") {
      return source.arrayBuffer();
    }
    if (source && typeof source.then === "function") {
      return source.then(function(resp) {
        if (resp && typeof resp.arrayBuffer === "function") {
          return resp.arrayBuffer();
        }
        return resp;
      });
    }
    return source;
  }

  function buildExports(instanceId) {
    var info = JSON.parse(__pi_wasm_get_exports_native(instanceId));
    var exports = {};
    for (var i = 0; i < info.length; i++) {
      var exp = info[i];
      if (exp.kind === "func") {
        (function(name) {
          exports[name] = function() {
            var args = [];
            for (var j = 0; j < arguments.length; j++) args.push(arguments[j]);
            return __pi_wasm_call_export_native(instanceId, name, args);
          };
        })(exp.name);
      } else if (exp.kind === "memory") {
        (function(name) {
          var memObj = Object.create(WebAssembly.Memory.prototype);
          Object.defineProperty(memObj, "buffer", {
            get: function() {
              __pi_wasm_get_buffer_native(instanceId, name);
              return globalThis.__pi_wasm_tmp_buf;
            },
            configurable: true
          });
          memObj.grow = function(delta) {
            var prevPages = __pi_wasm_memory_grow_native(instanceId, name, delta);
            if (prevPages < 0) {
              throw new RangeError("WebAssembly.Memory.grow(): failed to grow memory");
            }
            return prevPages;
          };
          exports[name] = memObj;
        })(exp.name);
      }
    }
    return exports;
  }

  globalThis.WebAssembly = {
    CompileError: CompileError,
    LinkError: LinkError,
    RuntimeError: RuntimeError,

    compile: function(source) {
      try {
        var bytes = normalizeBytes(source);
        var arr = [];
        for (var i = 0; i < bytes.length; i++) arr.push(bytes[i]);
        var moduleId = __pi_wasm_compile_native(arr);
        var wasmMod = { __wasm_module_id: moduleId };
        return syncResolve(wasmMod);
      } catch (e) {
        return syncReject(e);
      }
    },

    instantiate: function(source, _imports) {
      try {
        var moduleId;
        if (source && typeof source === "object" && source.__wasm_module_id !== undefined) {
          moduleId = source.__wasm_module_id;
        } else {
          var bytes = normalizeBytes(source);
          var arr = [];
          for (var i = 0; i < bytes.length; i++) arr.push(bytes[i]);
          moduleId = __pi_wasm_compile_native(arr);
        }
        var instanceId = __pi_wasm_instantiate_native(moduleId);
        var exports = buildExports(instanceId);
        var instance = { exports: exports };
        var wasmMod = { __wasm_module_id: moduleId };
        globalThis.__pi_wasm_last_instance_id = instanceId;
        instance.__pi_instance_id = instanceId;
        exports.__pi_instance_id = instanceId;

        if (source && typeof source === "object" && source.__wasm_module_id !== undefined) {
          return syncResolve(instance);
        }
        return syncResolve({ module: wasmMod, instance: instance });
      } catch (e) {
        return syncReject(e);
      }
    },

    validate: function(source) {
      var bytes = normalizeBytes(source);
      return __pi_wasm_validate_native(bytes);
    },

    instantiateStreaming: function(source, imports) {
      try {
        var bytesOrPromise = resolveStreamingBytes(source);
        if (bytesOrPromise && typeof bytesOrPromise.then === "function") {
          return bytesOrPromise.then(
            function(bytes) { return WebAssembly.instantiate(bytes, imports); },
            function(err) { return syncReject(err); }
          );
        }
        return WebAssembly.instantiate(bytesOrPromise, imports);
      } catch (e) {
        return syncReject(e);
      }
    },

    compileStreaming: function(source) {
      try {
        var bytesOrPromise = resolveStreamingBytes(source);
        if (bytesOrPromise && typeof bytesOrPromise.then === "function") {
          return bytesOrPromise.then(
            function(bytes) { return WebAssembly.compile(bytes); },
            function(err) { return syncReject(err); }
          );
        }
        return WebAssembly.compile(bytesOrPromise);
      } catch (e) {
        return syncReject(e);
      }
    },

    Memory: function(descriptor) {
      if (!(this instanceof WebAssembly.Memory)) {
        throw new TypeError("WebAssembly.Memory must be called with new");
      }
      var initial = descriptor && descriptor.initial !== undefined ? descriptor.initial : 0;
      var maximum = descriptor && descriptor.maximum !== undefined ? descriptor.maximum : undefined;
      var initialInt = Number(initial);
      if (!Number.isFinite(initialInt) || initialInt < 0 || Math.floor(initialInt) !== initialInt) {
        throw new RangeError("WebAssembly.Memory: invalid initial size");
      }
      var maxInt = maximum === undefined ? undefined : Number(maximum);
      if (maxInt !== undefined) {
        if (!Number.isFinite(maxInt) || maxInt < 0 || Math.floor(maxInt) !== maxInt) {
          throw new RangeError("WebAssembly.Memory: invalid maximum size");
        }
        if (maxInt < initialInt) {
          throw new RangeError("WebAssembly.Memory: maximum size smaller than initial");
        }
      }
      this._pages = initialInt;
      this._maximum = maxInt;
      this._buffer = new ArrayBuffer(this._pages * 65536);
      Object.defineProperty(this, "buffer", {
        get: function() { return this._buffer; },
        configurable: true
      });
      this.grow = function(delta) {
        var deltaInt = Number(delta);
        if (!Number.isFinite(deltaInt) || deltaInt < 0 || Math.floor(deltaInt) !== deltaInt) {
          throw new RangeError("WebAssembly.Memory.grow(): invalid delta");
        }
        var old = this._pages;
        var next = old + deltaInt;
        if (this._maximum !== undefined && next > this._maximum) {
          throw new RangeError("WebAssembly.Memory.grow(): maximum size exceeded");
        }
        var nextBuffer = new ArrayBuffer(next * 65536);
        new Uint8Array(nextBuffer).set(new Uint8Array(this._buffer));
        this._buffer = nextBuffer;
        this._pages = next;
        return old;
      };
    },

    Table: function(descriptor) {
      if (!(this instanceof WebAssembly.Table)) {
        throw new TypeError("WebAssembly.Table must be called with new");
      }
      var initial = descriptor && descriptor.initial !== undefined ? descriptor.initial : 0;
      var maximum = descriptor && descriptor.maximum !== undefined ? descriptor.maximum : undefined;
      var initialInt = Number(initial);
      if (!Number.isFinite(initialInt) || initialInt < 0 || Math.floor(initialInt) !== initialInt) {
        throw new RangeError("WebAssembly.Table: invalid initial size");
      }
      var maxInt = maximum === undefined ? undefined : Number(maximum);
      if (maxInt !== undefined) {
        if (!Number.isFinite(maxInt) || maxInt < 0 || Math.floor(maxInt) !== maxInt) {
          throw new RangeError("WebAssembly.Table: invalid maximum size");
        }
        if (maxInt < initialInt) {
          throw new RangeError("WebAssembly.Table: maximum size smaller than initial");
        }
      }
      var element = descriptor && descriptor.element ? String(descriptor.element) : "anyfunc";
      if (element !== "anyfunc" && element !== "funcref" && element !== "externref") {
        throw new TypeError("WebAssembly.Table: invalid element type");
      }
      this._element = element;
      this._maximum = maxInt;
      this._values = new Array(initialInt);
      for (var i = 0; i < initialInt; i++) this._values[i] = null;
      Object.defineProperty(this, "length", {
        get: function() { return this._values.length; },
        configurable: true
      });
      this.get = function(index) {
        var indexInt = Number(index);
        if (!Number.isFinite(indexInt) || indexInt < 0 || Math.floor(indexInt) !== indexInt) {
          throw new RangeError("WebAssembly.Table.get(): invalid index");
        }
        if (indexInt >= this._values.length) {
          throw new RangeError("WebAssembly.Table.get(): index out of bounds");
        }
        return this._values[indexInt];
      };
      this.set = function(index, value) {
        var indexInt = Number(index);
        if (!Number.isFinite(indexInt) || indexInt < 0 || Math.floor(indexInt) !== indexInt) {
          throw new RangeError("WebAssembly.Table.set(): invalid index");
        }
        if (indexInt >= this._values.length) {
          throw new RangeError("WebAssembly.Table.set(): index out of bounds");
        }
        this._values[indexInt] = value;
      };
      this.grow = function(delta, value) {
        var deltaInt = Number(delta);
        if (!Number.isFinite(deltaInt) || deltaInt < 0 || Math.floor(deltaInt) !== deltaInt) {
          throw new RangeError("WebAssembly.Table.grow(): invalid delta");
        }
        var old = this._values.length;
        var next = old + deltaInt;
        if (this._maximum !== undefined && next > this._maximum) {
          throw new RangeError("WebAssembly.Table.grow(): maximum size exceeded");
        }
        for (var i = old; i < next; i++) {
          this._values[i] = value === undefined ? null : value;
        }
        return old;
      };
    },

    Global: function(descriptor, value) {
      if (!(this instanceof WebAssembly.Global)) {
        throw new TypeError("WebAssembly.Global must be called with new");
      }
      var valType = descriptor && descriptor.value ? String(descriptor.value) : "i32";
      var mutable = descriptor && descriptor.mutable ? true : false;
      this._type = valType;
      this._mutable = mutable;
      this._value = value;
      Object.defineProperty(this, "value", {
        get: function() { return this._value; },
        set: function(next) {
          if (!this._mutable) {
            throw new TypeError("WebAssembly.Global is immutable");
          }
          this._value = next;
        },
        configurable: true
      });
      this.valueOf = function() { return this._value; };
    }
  };
})();
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a QuickJS runtime, inject WASM globals, and run a test.
    fn run_wasm_test(f: impl FnOnce(&Ctx<'_>, Rc<RefCell<WasmBridgeState>>)) {
        let rt = rquickjs::Runtime::new().expect("create runtime");
        let ctx = rquickjs::Context::full(&rt).expect("create context");
        ctx.with(|ctx| {
            let state = Rc::new(RefCell::new(WasmBridgeState::new()));
            inject_wasm_globals(&ctx, &state).expect("inject globals");
            f(&ctx, state);
        });
    }

    /// Get raw WASM binary bytes from WAT text.
    fn wat_to_wasm(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).expect("parse WAT to WASM binary")
    }

    #[test]
    fn js_to_i32_matches_javascript_wrapping_semantics() {
        assert_eq!(js_to_i32(2_147_483_648.0), -2_147_483_648);
        assert_eq!(js_to_i32(4_294_967_296.0), 0);
        assert_eq!(js_to_i32(-2_147_483_649.0), 2_147_483_647);
        assert_eq!(js_to_i32(-1.9), -1);
        assert_eq!(js_to_i32(1.9), 1);
        assert_eq!(js_to_i32(f64::NAN), 0);
        assert_eq!(js_to_i32(f64::INFINITY), 0);
        assert_eq!(js_to_i32(f64::NEG_INFINITY), 0);
    }

    #[test]
    fn compile_and_instantiate_trivial_module() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "add") (param i32 i32) (result i32)
                local.get 0 local.get 1 i32.add)
              (memory (export "memory") 1)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            // Store bytes as a JS array
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            // Compile
            let module_id: u32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("compile");
            assert!(module_id > 0);

            // Instantiate
            let instance_id: u32 = ctx
                .eval(format!("__pi_wasm_instantiate_native({module_id})"))
                .expect("instantiate");
            assert!(instance_id > 0);
        });
    }

    #[test]
    fn call_export_add() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "add") (param i32 i32) (result i32)
                local.get 0 local.get 1 i32.add)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: i32 = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    __pi_wasm_call_export_native(iid, "add", [3, 4]);
                "#,
                )
                .expect("call add");
            assert_eq!(result, 7);
        });
    }

    #[test]
    fn call_export_multiply() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "mul") (param i32 i32) (result i32)
                local.get 0 local.get 1 i32.mul)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: i32 = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    __pi_wasm_call_export_native(iid, "mul", [6, 7]);
                "#,
                )
                .expect("call mul");
            assert_eq!(result, 42);
        });
    }

    #[test]
    fn get_exports_lists_func_and_memory() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "f1") (result i32) i32.const 1)
              (func (export "f2") (param i32) (result i32) local.get 0)
              (memory (export "mem") 2)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let count: i32 = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    var exps = JSON.parse(__pi_wasm_get_exports_native(iid));
                    exps.length;
                ",
                )
                .expect("get exports count");
            assert_eq!(count, 3);
        });
    }

    #[test]
    fn get_exports_json_handles_escaped_names() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "name\"with_quote") (result i32) i32.const 1)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let name: String = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    JSON.parse(__pi_wasm_get_exports_native(iid))[0].name;
                ",
                )
                .expect("parse export JSON");
            assert_eq!(name, "name\"with_quote");
        });
    }

    #[test]
    fn memory_buffer_returns_arraybuffer() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let size: i32 = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    var len = __pi_wasm_get_buffer_native(iid, "memory");
                    len;
                "#,
                )
                .expect("get buffer size");
            // 1 page = 64 KiB = 65536 bytes
            assert_eq!(size, 65536);

            // Verify the ArrayBuffer was stored in the global
            let buf_size: i32 = ctx
                .eval("__pi_wasm_tmp_buf.byteLength")
                .expect("tmp buffer size");
            assert_eq!(buf_size, 65536);
        });
    }

    #[test]
    fn memory_grow_succeeds() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1 10))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let prev: i32 = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    __pi_wasm_memory_grow_native(iid, "memory", 2);
                "#,
                )
                .expect("grow memory");
            // Previous size was 1 page
            assert_eq!(prev, 1);

            let new_size: i32 = ctx
                .eval(r#"__pi_wasm_memory_size_native(iid, "memory")"#)
                .expect("memory size");
            assert_eq!(new_size, 3);
        });
    }

    #[test]
    fn memory_grow_denied_by_policy() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1))"#);
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let instance_id: u32 = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
                )
                .expect("instantiate");

            // Reduce max pages to 2 in the instance's store
            {
                let mut bridge = state.borrow_mut();
                let inst = bridge.instances.get_mut(&instance_id).unwrap();
                inst.store.data_mut().max_memory_pages = 2;
            }

            // Try to grow by 5 pages → should be denied (1 + 5 > 2)
            let result: i32 = ctx
                .eval(format!(
                    "__pi_wasm_memory_grow_native({instance_id}, 'memory', 5)"
                ))
                .expect("grow denied");
            assert_eq!(result, -1);
        });
    }

    #[test]
    fn compile_invalid_bytes_fails() {
        run_wasm_test(|ctx, _state| {
            let result: rquickjs::Result<u32> = ctx.eval("__pi_wasm_compile_native([0, 1, 2, 3])");
            assert!(result.is_err());
        });
    }

    #[test]
    fn js_polyfill_webassembly_validate_accepts_valid_module() {
        let wasm_bytes = wat_to_wasm(r"(module)");
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: bool = ctx
                .eval("WebAssembly.validate(__test_bytes)")
                .expect("validate");
            assert!(result);
        });
    }

    #[test]
    fn js_polyfill_webassembly_validate_rejects_invalid_module() {
        run_wasm_test(|ctx, _state| {
            let result: bool = ctx
                .eval("WebAssembly.validate([0, 1, 2, 3])")
                .expect("validate");
            assert!(!result);
        });
    }

    #[test]
    fn js_polyfill_webassembly_validate_does_not_register_module() {
        let wasm_bytes = wat_to_wasm(r"(module)");
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let before = state.borrow().modules.len();
            let result: bool = ctx
                .eval("WebAssembly.validate(__test_bytes)")
                .expect("validate");
            let after = state.borrow().modules.len();

            assert!(result);
            assert_eq!(before, after);
        });
    }

    #[test]
    fn js_polyfill_webassembly_table_basic() {
        run_wasm_test(|ctx, _state| {
            ctx.eval::<(), _>(
                r#"
                const table = new WebAssembly.Table({ element: "funcref", initial: 2, maximum: 3 });
                if (table.length !== 2) throw new Error("table length mismatch");
                table.set(0, function() { return 1; });
                if (typeof table.get(0) !== "function") throw new Error("table get failed");
                const old = table.grow(1);
                if (old !== 2 || table.length !== 3) throw new Error("table grow failed");
                let threw = false;
                try { table.grow(1); } catch (e) { threw = true; }
                if (!threw) throw new Error("table maximum not enforced");
                "#,
            )
            .expect("table polyfill");
        });
    }

    #[test]
    fn js_polyfill_webassembly_table_validation() {
        run_wasm_test(|ctx, _state| {
            let result: bool = ctx
                .eval(
                    r#"
                    (() => {
                        const table = new WebAssembly.Table({ element: "funcref", initial: 1, maximum: 2 });
                        let ok = false;
                        try { table.get(0.5); } catch (e) { ok = e instanceof RangeError; }
                        if (!ok) return false;
                        ok = false;
                        try { table.grow(1.25); } catch (e) { ok = e instanceof RangeError; }
                        if (!ok) return false;
                        ok = false;
                        try { table.grow(2); } catch (e) { ok = e instanceof RangeError; }
                        return ok;
                    })()
                    "#,
                )
                .expect("table validation");
            assert!(result);
        });
    }

    #[test]
    fn js_polyfill_webassembly_memory_validation() {
        run_wasm_test(|ctx, _state| {
            let result: bool = ctx
                .eval(
                    r"
                    (() => {
                        const mem = new WebAssembly.Memory({ initial: 1, maximum: 1 });
                        let ok = false;
                        try { mem.grow(0.5); } catch (e) { ok = e instanceof RangeError; }
                        if (!ok) return false;
                        ok = false;
                        try { mem.grow(1); } catch (e) { ok = e instanceof RangeError; }
                        return ok;
                    })()
                    ",
                )
                .expect("memory validation");
            assert!(result);
        });
    }

    #[test]
    fn js_polyfill_webassembly_global_basic() {
        run_wasm_test(|ctx, _state| {
            ctx.eval::<(), _>(
                r#"
                const g = new WebAssembly.Global({ value: "i32", mutable: true }, 1);
                if (g.value !== 1) throw new Error("global value mismatch");
                g.value = 2;
                if (g.value !== 2) throw new Error("global set failed");
                const imm = new WebAssembly.Global({ value: "i32" }, 7);
                let threw = false;
                try { imm.value = 9; } catch (e) { threw = true; }
                if (!threw) throw new Error("immutable global should throw");
                "#,
            )
            .expect("global polyfill");
        });
    }

    #[test]
    fn instantiate_nonexistent_module_fails() {
        run_wasm_test(|ctx, _state| {
            let result: rquickjs::Result<u32> = ctx.eval("__pi_wasm_instantiate_native(99999)");
            assert!(result.is_err());
        });
    }

    #[test]
    fn compile_rejects_when_module_limit_reached() {
        let wasm_bytes = wat_to_wasm(r"(module)");
        run_wasm_test(|ctx, state| {
            state.borrow_mut().set_limits_for_test(1, 8);

            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let first: u32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("first compile");
            assert!(first > 0);

            let second: rquickjs::Result<u32> = ctx.eval("__pi_wasm_compile_native(__test_bytes)");
            assert!(second.is_err());
        });
    }

    #[test]
    fn instantiate_rejects_when_instance_limit_reached() {
        let wasm_bytes = wat_to_wasm(r"(module)");
        run_wasm_test(|ctx, state| {
            state.borrow_mut().set_limits_for_test(8, 1);

            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let module_id: u32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("compile");

            let first: u32 = ctx
                .eval(format!("__pi_wasm_instantiate_native({module_id})"))
                .expect("first instantiate");
            assert!(first > 0);

            let second: rquickjs::Result<u32> =
                ctx.eval(format!("__pi_wasm_instantiate_native({module_id})"));
            assert!(second.is_err());
        });
    }

    #[test]
    fn alloc_id_skips_zero_on_wrap() {
        let wasm_bytes = wat_to_wasm(r"(module)");
        run_wasm_test(|ctx, state| {
            {
                let mut bridge = state.borrow_mut();
                bridge.set_limits_for_test(8, 8);
                bridge.next_id = MAX_JS_WASM_ID;
            }

            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let first: i32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("first compile");
            let second: i32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("second compile");

            assert_eq!(first, i32::MAX);
            assert_eq!(second, 1);
        });
    }

    #[test]
    fn call_nonexistent_export_fails() {
        let wasm_bytes = wat_to_wasm(r#"(module (func (export "f") (result i32) i32.const 1))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<i32> = ctx.eval(
                r#"
                var mid = __pi_wasm_compile_native(__test_bytes);
                var iid = __pi_wasm_instantiate_native(mid);
                __pi_wasm_call_export_native(iid, "nonexistent", []);
            "#,
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn call_export_i64_param_is_rejected() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "id64") (param i64) (result i64)
                local.get 0)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<i32> = ctx.eval(
                r#"
                var mid = __pi_wasm_compile_native(__test_bytes);
                var iid = __pi_wasm_instantiate_native(mid);
                __pi_wasm_call_export_native(iid, "id64", [1]);
            "#,
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn call_export_i64_result_is_rejected() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "ret64") (result i64)
                i64.const 42)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<i32> = ctx.eval(
                r#"
                var mid = __pi_wasm_compile_native(__test_bytes);
                var iid = __pi_wasm_instantiate_native(mid);
                __pi_wasm_call_export_native(iid, "ret64", []);
            "#,
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn call_export_multivalue_result_is_rejected() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "pair") (result i32 i32)
                i32.const 1
                i32.const 2)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<i32> = ctx.eval(
                r#"
                var mid = __pi_wasm_compile_native(__test_bytes);
                var iid = __pi_wasm_instantiate_native(mid);
                __pi_wasm_call_export_native(iid, "pair", []);
            "#,
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn call_export_externref_result_is_rejected() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "retref") (result externref)
                ref.null extern)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<i32> = ctx.eval(
                r#"
                var mid = __pi_wasm_compile_native(__test_bytes);
                var iid = __pi_wasm_instantiate_native(mid);
                __pi_wasm_call_export_native(iid, "retref", []);
            "#,
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn js_polyfill_webassembly_instantiate() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "add") (param i32 i32) (result i32)
                local.get 0 local.get 1 i32.add)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            // Use the full JS polyfill API (synchronous for QuickJS)
            let has_wa: bool = ctx
                .eval("typeof globalThis.WebAssembly !== 'undefined'")
                .expect("check WebAssembly");
            assert!(has_wa);

            // WebAssembly.instantiate returns a Promise; in QuickJS we can
            // resolve it synchronously via .then()
            let result: i32 = ctx
                .eval(
                    r"
                    var __test_result = -1;
                    WebAssembly.instantiate(__test_bytes).then(function(r) {
                        __test_result = r.instance.exports.add(10, 20);
                    });
                    __test_result;
                ",
                )
                .expect("polyfill instantiate");
            assert_eq!(result, 30);
        });
    }

    #[test]
    fn js_polyfill_webassembly_compile_streaming() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "add") (param i32 i32) (result i32)
                local.get 0 local.get 1 i32.add)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: i32 = ctx
                .eval(
                    r"
                    var __test_result = -1;
                    var __resp = { arrayBuffer: function() { return new Uint8Array(__test_bytes).buffer; } };
                    WebAssembly.compileStreaming(__resp).then(function(mod) {
                        WebAssembly.instantiate(mod).then(function(r) {
                            __test_result = r.exports.add(2, 3);
                        });
                    });
                    __test_result;
                ",
                )
                .expect("polyfill compileStreaming");
            assert_eq!(result, 5);
        });
    }

    #[test]
    fn js_polyfill_webassembly_instantiate_streaming() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (func (export "add") (param i32 i32) (result i32)
                local.get 0 local.get 1 i32.add)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: i32 = ctx
                .eval(
                    r"
                    var __test_result = -1;
                    var __resp = { arrayBuffer: function() { return new Uint8Array(__test_bytes).buffer; } };
                    WebAssembly.instantiateStreaming(__resp).then(function(r) {
                        __test_result = r.instance.exports.add(6, 7);
                    });
                    __test_result;
                ",
                )
                .expect("polyfill instantiateStreaming");
            assert_eq!(result, 13);
        });
    }

    #[test]
    fn js_polyfill_memory_buffer_getter() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let size: i32 = ctx
                .eval(
                    r"
                    var __test_size = -1;
                    WebAssembly.instantiate(__test_bytes).then(function(r) {
                        __test_size = r.instance.exports.memory.buffer.byteLength;
                    });
                    __test_size;
                ",
                )
                .expect("polyfill memory buffer");
            assert_eq!(size, 65536);
        });
    }

    #[test]
    fn js_polyfill_exported_memory_is_webassembly_memory() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let is_memory: bool = ctx
                .eval(
                    r"
                    var __is_memory = false;
                    WebAssembly.instantiate(__test_bytes).then(function(r) {
                        __is_memory = r.instance.exports.memory instanceof WebAssembly.Memory;
                    });
                    __is_memory;
                ",
                )
                .expect("exported memory instanceof WebAssembly.Memory");
            assert!(is_memory);
        });
    }

    #[test]
    fn js_polyfill_memory_grow_returns_previous_pages() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1 10))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let prev_pages: i32 = ctx
                .eval(
                    r"
                    var __test_prev = -1;
                    WebAssembly.instantiate(__test_bytes).then(function(r) {
                        __test_prev = r.instance.exports.memory.grow(2);
                    });
                    __test_prev;
                ",
                )
                .expect("polyfill memory grow");
            assert_eq!(prev_pages, 1);

            let new_size: i32 = ctx
                .eval(
                    r"
                    var __test_size = -1;
                    WebAssembly.instantiate(__test_bytes).then(function(r) {
                        r.instance.exports.memory.grow(2);
                        __test_size = r.instance.exports.memory.buffer.byteLength;
                    });
                    __test_size;
                ",
                )
                .expect("polyfill memory size after grow");
            assert_eq!(new_size, 3 * 65536);
        });
    }

    #[test]
    fn js_polyfill_memory_grow_failure_throws_range_error() {
        let wasm_bytes = wat_to_wasm(r#"(module (memory (export "memory") 1 1))"#);
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let threw_range_error: bool = ctx
                .eval(
                    r"
                    var __threw_range_error = false;
                    WebAssembly.instantiate(__test_bytes).then(function(r) {
                        try {
                            r.instance.exports.memory.grow(1);
                        } catch (e) {
                            __threw_range_error = e instanceof RangeError;
                        }
                    });
                    __threw_range_error;
                ",
                )
                .expect("polyfill memory grow failure");
            assert!(threw_range_error);
        });
    }

    #[test]
    fn js_memory_constructor_grow_preserves_existing_bytes() {
        run_wasm_test(|ctx, _state| {
            let summary: String = ctx
                .eval(
                    r#"
                    var mem = new WebAssembly.Memory({ initial: 1 });
                    var before = new Uint8Array(mem.buffer);
                    before[0] = 7;
                    before[65535] = 9;
                    var prev = mem.grow(1);
                    var after = new Uint8Array(mem.buffer);
                    [prev, after.byteLength, after[0], after[65535], after[65536]].join(",");
                "#,
                )
                .expect("memory constructor grow preserves bytes");
            assert_eq!(summary, "1,131072,7,9,0");
        });
    }

    #[test]
    fn module_with_unknown_import_fails_closed() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "log" (func (param i32)))
              (func (export "run") (result i32)
                i32.const 42
                call 0
                i32.const 1)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<u32> = ctx.eval(
                r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn module_with_explicit_compat_stub_import_instantiates() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "__syscall_ioctl" (func $ioctl (param i32 i32 i32) (result i32)))
              (func (export "call_ioctl") (result i32)
                i32.const 0
                i32.const 0
                i32.const 0
                call $ioctl)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: i32 = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    __pi_wasm_call_export_native(iid, "call_ioctl", []);
                "#,
                )
                .expect("call explicit compatibility stub");
            assert_eq!(result, 0);
        });
    }

    #[test]
    fn module_with_non_function_import_fails_closed() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "memory" (memory 1))
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let result: rquickjs::Result<u32> = ctx.eval(
                r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
            );
            assert!(result.is_err());
        });
    }

    #[test]
    fn native_memory_helpers_round_trip_live_wasm_memory() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (memory (export "memory") 1)
              (func (export "read32") (param i32) (result i32)
                local.get 0
                i32.load)
              (func (export "write32") (param i32 i32)
                local.get 0
                local.get 1
                i32.store)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let summary: String = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    __pi_wasm_memory_write_native(iid, "memory", 32, [1, 2, 3, 4]);
                    var readBack = __pi_wasm_call_export_native(iid, "read32", [32]);
                    __pi_wasm_call_export_native(iid, "write32", [40, 0x11223344]);
                    var bytes = __pi_wasm_memory_read_native(iid, "memory", 40, 4);
                    [readBack, bytes[0], bytes[1], bytes[2], bytes[3]].join(",");
                "#,
                )
                .expect("memory helpers round-trip");
            assert_eq!(summary, "67305985,68,51,34,17");
        });
    }

    #[test]
    fn staged_file_host_imports_can_open_and_read_wad_bytes() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_close" (func $fd_close (param i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/doom/doom1.wad\00")
              (func (export "readfirst4") (result i32)
                (local $fd i32)
                i32.const 96
                i32.const 128
                i32.store
                i32.const 100
                i32.const 4
                i32.store
                i32.const -100
                i32.const 64
                i32.const 0
                i32.const 0
                call $openat
                local.tee $fd
                i32.const 96
                i32.const 1
                i32.const 104
                call $fd_read
                drop
                local.get $fd
                call $fd_close
                drop
                i32.const 128
                i32.load)
              (func (export "bytes_read") (result i32)
                i32.const 104
                i32.load)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let summary: String = ctx
                .eval(
                    r#"
                    __pi_wasm_stage_file_native("/doom/doom1.wad", [1, 2, 3, 4]);
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    var first = __pi_wasm_call_export_native(iid, "readfirst4", []);
                    var bytes = __pi_wasm_call_export_native(iid, "bytes_read", []);
                    [first, bytes].join(",");
                "#,
                )
                .expect("staged file read");
            assert_eq!(summary, "67305985,4");
        });
    }

    #[test]
    fn staged_file_host_imports_can_create_and_write_virtual_files() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/tmp/out.bin\00")
              (data (i32.const 160) "\05\06\07")
              (func (export "write_new") (result i32)
                (local $fd i32)
                i32.const 128
                i32.const 160
                i32.store
                i32.const 132
                i32.const 3
                i32.store
                i32.const -100
                i32.const 64
                i32.const 577
                i32.const 0
                call $openat
                local.set $fd
                local.get $fd
                i32.const 128
                i32.const 1
                i32.const 136
                call $fd_write
                drop
                i32.const 136
                i32.load)
            )"#,
        );
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let instance_id: u32 = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
                )
                .expect("instantiate writable virtual file module");

            let bytes_written: i32 = ctx
                .eval(format!(
                    r#"__pi_wasm_call_export_native({instance_id}, "write_new", [])"#
                ))
                .expect("write newly created virtual file");
            assert_eq!(bytes_written, 3);

            let bridge = state.borrow();
            let contents = bridge
                .instances
                .get(&instance_id)
                .and_then(|inst| inst.store.data().staged_files.get("/tmp/out.bin"))
                .map(|arc| (**arc).clone());
            assert_eq!(contents, Some(vec![5, 6, 7]));
        });
    }

    #[test]
    fn staged_file_host_imports_honor_truncate_flag_for_writes() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/tmp/existing.bin\00")
              (data (i32.const 160) "\09")
              (func (export "truncate_then_write") (result i32)
                (local $fd i32)
                i32.const 128
                i32.const 160
                i32.store
                i32.const 132
                i32.const 1
                i32.store
                i32.const -100
                i32.const 64
                i32.const 513
                i32.const 0
                call $openat
                local.set $fd
                local.get $fd
                i32.const 128
                i32.const 1
                i32.const 136
                call $fd_write
                drop
                i32.const 136
                i32.load)
            )"#,
        );
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let instance_id: u32 = ctx
                .eval(
                    r#"
                    __pi_wasm_stage_file_native("/tmp/existing.bin", [1, 2, 3, 4]);
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                "#,
                )
                .expect("instantiate truncate virtual file module");

            let bytes_written: i32 = ctx
                .eval(format!(
                    r#"__pi_wasm_call_export_native({instance_id}, "truncate_then_write", [])"#
                ))
                .expect("truncate existing virtual file");
            assert_eq!(bytes_written, 1);

            let bridge = state.borrow();
            let contents = bridge
                .instances
                .get(&instance_id)
                .and_then(|inst| inst.store.data().staged_files.get("/tmp/existing.bin"))
                .map(|arc| (**arc).clone());
            assert_eq!(contents, Some(vec![9]));
        });
    }

    #[test]
    fn staged_file_host_imports_reject_write_on_read_only_descriptor() {
        let wasm_bytes = wat_to_wasm(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/doom/doom1.wad\00")
              (data (i32.const 160) "\09\08")
              (func (export "write_read_only") (result i32)
                (local $fd i32)
                i32.const 128
                i32.const 160
                i32.store
                i32.const 132
                i32.const 2
                i32.store
                i32.const -100
                i32.const 64
                i32.const 0
                i32.const 0
                call $openat
                local.set $fd
                local.get $fd
                i32.const 128
                i32.const 1
                i32.const 136
                call $fd_write)
              (func (export "bytes_written") (result i32)
                i32.const 136
                i32.load)
            )"#,
        );
        run_wasm_test(|ctx, _state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let summary: String = ctx
                .eval(
                    r#"
                    __pi_wasm_stage_file_native("/doom/doom1.wad", [1, 2, 3, 4]);
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    var result = __pi_wasm_call_export_native(iid, "write_read_only", []);
                    var bytes = __pi_wasm_call_export_native(iid, "bytes_written", []);
                    [result, bytes].join(",");
                "#,
                )
                .expect("read-only descriptor write rejection");
            assert_eq!(summary, "8,0");
        });
    }

    #[test]
    fn staged_file_host_imports_reject_write_past_virtual_file_limit() {
        let wasm_bytes = wat_to_wasm(&format!(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_seek" (func $fd_seek (param i32 i64 i32 i32) (result i32)))
              (import "env" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/tmp/too-big.bin\00")
              (data (i32.const 160) "\07")
              (func (export "seek_then_write_too_large") (result i32)
                (local $fd i32)
                i32.const 128
                i32.const 160
                i32.store
                i32.const 132
                i32.const 1
                i32.store
                i32.const -100
                i32.const 64
                i32.const 577
                i32.const 0
                call $openat
                local.set $fd
                local.get $fd
                i64.const {MAX_VIRTUAL_FILE_BYTES}
                i32.const 0
                i32.const 144
                call $fd_seek
                drop
                local.get $fd
                i32.const 128
                i32.const 1
                i32.const 136
                call $fd_write)
              (func (export "bytes_written") (result i32)
                i32.const 136
                i32.load)
            )"#,
        ));
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let instance_id: u32 = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
                )
                .expect("instantiate large seek virtual file module");

            let summary: String = ctx
                .eval(format!(
                    r#"
                    var result = __pi_wasm_call_export_native({instance_id}, "seek_then_write_too_large", []);
                    var bytes = __pi_wasm_call_export_native({instance_id}, "bytes_written", []);
                    [result, bytes].join(",");
                "#
                ))
                .expect("reject oversize virtual file write");
            assert_eq!(summary, "27,0");

            let bridge = state.borrow();
            let len = bridge
                .instances
                .get(&instance_id)
                .and_then(|inst| inst.store.data().staged_files.get("/tmp/too-big.bin"))
                .map(|v| v.len());
            assert_eq!(len, Some(0));
        });
    }

    #[test]
    fn staged_file_host_imports_reject_multi_iov_limit_overflow_atomically() {
        let near_limit = MAX_VIRTUAL_FILE_BYTES - 1;
        let wasm_bytes = wat_to_wasm(&format!(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_seek" (func $fd_seek (param i32 i64 i32 i32) (result i32)))
              (import "env" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/tmp/too-big-split.bin\00")
              (data (i32.const 160) "\07\08")
              (func (export "split_write_too_large") (result i32)
                (local $fd i32)
                i32.const 128
                i32.const 160
                i32.store
                i32.const 132
                i32.const 1
                i32.store
                i32.const 136
                i32.const 161
                i32.store
                i32.const 140
                i32.const 1
                i32.store
                i32.const -100
                i32.const 64
                i32.const 577
                i32.const 0
                call $openat
                local.set $fd
                local.get $fd
                i64.const {near_limit}
                i32.const 0
                i32.const 152
                call $fd_seek
                drop
                local.get $fd
                i32.const 128
                i32.const 2
                i32.const 144
                call $fd_write)
              (func (export "bytes_written") (result i32)
                i32.const 144
                i32.load)
            )"#,
        ));
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let instance_id: u32 = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
                )
                .expect("instantiate split oversize virtual file module");

            let summary: String = ctx
                .eval(format!(
                    r#"
                    var result = __pi_wasm_call_export_native({instance_id}, "split_write_too_large", []);
                    var bytes = __pi_wasm_call_export_native({instance_id}, "bytes_written", []);
                    [result, bytes].join(",");
                "#
                ))
                .expect("reject split oversize virtual file write");
            assert_eq!(summary, "27,0");

            let bridge = state.borrow();
            let len = bridge
                .instances
                .get(&instance_id)
                .and_then(|inst| inst.store.data().staged_files.get("/tmp/too-big-split.bin"))
                .map(|v| v.len());
            assert_eq!(len, Some(0));
        });
    }

    #[test]
    fn staged_file_host_imports_allow_zero_length_write_past_virtual_file_limit() {
        let past_limit = MAX_VIRTUAL_FILE_BYTES + 1;
        let wasm_bytes = wat_to_wasm(&format!(
            r#"(module
              (import "env" "__syscall_openat" (func $openat (param i32 i32 i32 i32) (result i32)))
              (import "env" "fd_seek" (func $fd_seek (param i32 i64 i32 i32) (result i32)))
              (import "env" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 1)
              (data (i32.const 64) "/tmp/too-big-zero.bin\00")
              (func (export "zero_write_after_large_seek") (result i32)
                (local $fd i32)
                i32.const 128
                i32.const 160
                i32.store
                i32.const 132
                i32.const 0
                i32.store
                i32.const -100
                i32.const 64
                i32.const 577
                i32.const 0
                call $openat
                local.set $fd
                local.get $fd
                i64.const {past_limit}
                i32.const 0
                i32.const 144
                call $fd_seek
                drop
                local.get $fd
                i32.const 128
                i32.const 1
                i32.const 136
                call $fd_write)
              (func (export "bytes_written") (result i32)
                i32.const 136
                i32.load)
            )"#,
        ));
        run_wasm_test(|ctx, state| {
            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let instance_id: u32 = ctx
                .eval(
                    r"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    __pi_wasm_instantiate_native(mid);
                ",
                )
                .expect("instantiate zero-write virtual file module");

            let summary: String = ctx
                .eval(format!(
                    r#"
                    var result = __pi_wasm_call_export_native({instance_id}, "zero_write_after_large_seek", []);
                    var bytes = __pi_wasm_call_export_native({instance_id}, "bytes_written", []);
                    [result, bytes].join(",");
                "#
                ))
                .expect("allow zero-length write after large seek");
            assert_eq!(summary, "0,0");

            let bridge = state.borrow();
            let len = bridge
                .instances
                .get(&instance_id)
                .and_then(|inst| inst.store.data().staged_files.get("/tmp/too-big-zero.bin"))
                .map(|v| v.len());
            assert_eq!(len, Some(0));
        });
    }
}
