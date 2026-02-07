//! PiWasm: WebAssembly polyfill for QuickJS runtime.
//!
//! Provides `globalThis.WebAssembly` inside QuickJS, backed by wasmtime.
//! Enables JS extensions to use WebAssembly modules (e.g., Emscripten-compiled
//! code) even though QuickJS lacks native WebAssembly support.
//!
//! Architecture:
//! - Native Rust functions (`__pi_wasm_*`) handle compile/instantiate/call
//! - A JS polyfill wraps them into the standard `WebAssembly` namespace
//! - Memory is synced as ArrayBuffer snapshots (wasmtime → JS) via a getter

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rquickjs::function::Func;
use rquickjs::{ArrayBuffer, Ctx, Value};
use serde::Serialize;
use tracing::debug;
use wasmtime::{
    Caller, Engine, ExternType, Instance as WasmInstance, Linker, Module as WasmModule, Store, Val,
    ValType,
};

// ---------------------------------------------------------------------------
// Bridge state
// ---------------------------------------------------------------------------

/// Host data stored in each wasmtime `Store`.
struct WasmHostData {
    /// Maximum memory pages allowed (enforced on grow).
    max_memory_pages: u64,
}

/// Per-instance state: the wasmtime `Store` owns all WASM objects.
struct InstanceState {
    store: Store<WasmHostData>,
    instance: WasmInstance,
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
            next_id: 1,
            max_modules: DEFAULT_MAX_MODULES,
            max_instances: DEFAULT_MAX_INSTANCES,
        }
    }

    fn alloc_id(&mut self) -> Result<u32, String> {
        let start = self.next_id.max(1);
        let mut candidate = start;

        loop {
            if !self.modules.contains_key(&candidate) && !self.instances.contains_key(&candidate) {
                self.next_id = candidate.wrapping_add(1);
                if self.next_id == 0 {
                    self.next_id = 1;
                }
                return Ok(candidate);
            }

            candidate = candidate.wrapping_add(1);
            if candidate == 0 {
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
    }
    // Try array of numbers
    if let Some(arr) = value.as_array() {
        let mut bytes = Vec::with_capacity(arr.len());
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
/// All WASM numeric types (i32, i64, f32, f64) are representable as f64.
#[allow(clippy::cast_precision_loss)]
fn val_to_f64(val: &Val) -> f64 {
    match val {
        Val::I32(v) => f64::from(*v),
        Val::I64(v) => *v as f64,
        Val::F32(bits) => f64::from(f32::from_bits(*bits)),
        Val::F64(bits) => f64::from_bits(*bits),
        _ => 0.0,
    }
}

#[allow(clippy::cast_possible_truncation)]
fn js_to_val(ctx: &Ctx<'_>, value: &Value<'_>, ty: &ValType) -> rquickjs::Result<Val> {
    match ty {
        ValType::I32 => {
            let v: f64 = value
                .as_number()
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Expected number for i32"))?;
            Ok(Val::I32(v as i32))
        }
        ValType::I64 => {
            let v: f64 = value
                .as_number()
                .ok_or_else(|| throw_wasm(ctx, "TypeError", "Expected number for i64"))?;
            Ok(Val::I64(v as i64))
        }
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

// ---------------------------------------------------------------------------
// Import stubs
// ---------------------------------------------------------------------------

/// Register no-op stub functions for every function import the module requires.
/// Non-function imports cause an error (memory/table/global imports not yet
/// supported for the polyfill).
fn register_stub_imports(
    linker: &mut Linker<WasmHostData>,
    module: &WasmModule,
) -> Result<(), String> {
    for import in module.imports() {
        let mod_name = import.module();
        let imp_name = import.name();
        if let ExternType::Func(func_ty) = import.ty() {
            let result_types: Vec<ValType> = func_ty.results().collect();
            linker
                .func_new(
                    mod_name,
                    imp_name,
                    func_ty.clone(),
                    move |_caller: Caller<'_, WasmHostData>,
                          _params: &[Val],
                          results: &mut [Val]| {
                        for (i, ty) in result_types.iter().enumerate() {
                            results[i] = Val::default_for_ty(ty).unwrap_or(Val::I32(0));
                        }
                        Ok(())
                    },
                )
                .map_err(|e| format!("Failed to stub import {mod_name}.{imp_name}: {e}"))?;
        } else {
            // Non-function imports are currently skipped for MVP.
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
                    register_stub_imports(&mut linker, &module)
                        .map_err(|e| throw_wasm(&ctx, "LinkError", &e))?;

                    let mut store = Store::new(
                        &bridge.engine,
                        WasmHostData {
                            max_memory_pages: DEFAULT_MAX_MEMORY_PAGES,
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

                    let func = inst
                        .instance
                        .get_func(&mut inst.store, &name)
                        .ok_or_else(|| {
                            throw_wasm(&ctx, "RuntimeError", &format!("Export '{name}' not found"))
                        })?;

                    let func_ty = func.ty(&inst.store);
                    let param_types: Vec<ValType> = func_ty.params().collect();

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
                    let mut results: Vec<Val> = result_types
                        .iter()
                        .map(|ty| Val::default_for_ty(ty).unwrap_or(Val::I32(0)))
                        .collect();

                    func.call(&mut inst.store, &params, &mut results)
                        .map_err(|e| throw_wasm(&ctx, "RuntimeError", &e.to_string()))?;

                    // Return first result as f64 (covers i32, i64, f32, f64)
                    Ok(results.first().map_or(0.0, val_to_f64))
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
                    let memory = inst
                        .instance
                        .get_memory(&mut inst.store, &mem_name)
                        .ok_or_else(|| throw_wasm(&ctx, "RuntimeError", "Memory not found"))?;
                    let data = memory.data(&inst.store);
                    let len = i32::try_from(data.len()).unwrap_or(i32::MAX);
                    let buffer = ArrayBuffer::new_copy(ctx.clone(), data)?;
                    ctx.globals().set("__pi_wasm_tmp_buf", buffer)?;
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
          var memObj = {};
          Object.defineProperty(memObj, "buffer", {
            get: function() {
              __pi_wasm_get_buffer_native(instanceId, name);
              return globalThis.__pi_wasm_tmp_buf;
            },
            configurable: true
          });
          memObj.grow = function(delta) {
            return __pi_wasm_memory_grow_native(instanceId, name, delta);
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

        if (source && typeof source === "object" && source.__wasm_module_id !== undefined) {
          return syncResolve(instance);
        }
        return syncResolve({ module: wasmMod, instance: instance });
      } catch (e) {
        return syncReject(e);
      }
    },

    validate: function(_bytes) {
      throw new Error("WebAssembly.validate not yet supported in PiJS");
    },

    instantiateStreaming: function() {
      throw new Error("WebAssembly.instantiateStreaming not supported in PiJS");
    },

    compileStreaming: function() {
      throw new Error("WebAssembly.compileStreaming not supported in PiJS");
    },

    Memory: function(descriptor) {
      if (!(this instanceof WebAssembly.Memory)) {
        throw new TypeError("WebAssembly.Memory must be called with new");
      }
      var initial = descriptor && descriptor.initial ? descriptor.initial : 0;
      this._pages = initial;
      this._buffer = new ArrayBuffer(initial * 65536);
      Object.defineProperty(this, "buffer", {
        get: function() { return this._buffer; },
        configurable: true
      });
      this.grow = function(delta) {
        var old = this._pages;
        this._pages += delta;
        this._buffer = new ArrayBuffer(this._pages * 65536);
        return old;
      };
    },

    Table: function() {
      throw new Error("WebAssembly.Table not yet supported in PiJS");
    },

    Global: function() {
      throw new Error("WebAssembly.Global not yet supported in PiJS");
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
                bridge.next_id = u32::MAX;
            }

            let arr = rquickjs::Array::new(ctx.clone()).unwrap();
            for (i, &b) in wasm_bytes.iter().enumerate() {
                arr.set(i, i32::from(b)).unwrap();
            }
            ctx.globals().set("__test_bytes", arr).unwrap();

            let first: u32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("first compile");
            let second: u32 = ctx
                .eval("__pi_wasm_compile_native(__test_bytes)")
                .expect("second compile");

            assert_eq!(first, u32::MAX);
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
    fn module_with_imports_instantiates_with_stubs() {
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

            let result: i32 = ctx
                .eval(
                    r#"
                    var mid = __pi_wasm_compile_native(__test_bytes);
                    var iid = __pi_wasm_instantiate_native(mid);
                    __pi_wasm_call_export_native(iid, "run", []);
                "#,
                )
                .expect("call with import stubs");
            assert_eq!(result, 1);
        });
    }
}
