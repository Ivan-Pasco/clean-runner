//! Clean Language WASM execution for non-HTTP contexts (scripts, CLI tools,
//! compiler integration tests). It is NOT a web server.
//!
//! Host functions come from the shared `host-bridge` crate — the same
//! implementations clean-server links — so the runner never drifts from the
//! bridge ABI in `foundation/spec/platform/HOST_BRIDGE.md`. Server-only
//! (Layer 3) functions such as request context and routing are not provided;
//! modules that import them fail at instantiation with the missing name.

use host_bridge::wasm_linker::{register_all_functions, WasmState};
use std::fs;
use wasmtime::{Engine, Linker, Module, Store};

/// Entry points tried in order, matching clean-server's initialization.
const ENTRY_POINTS: [&str; 4] = ["start", "_start", "main", "_main"];

/// Functions the function registry requires on every host (`hosts = ["all"]`)
/// that `host-bridge` does not provide because they are not Layer 2.
fn register_runner_functions(
    linker: &mut Linker<WasmState>,
) -> Result<(), Box<dyn std::error::Error>> {
    // _server_sleep(ms: i64) -> void — suspend execution for N milliseconds.
    linker.func_wrap("env", "_server_sleep", |ms: i64| {
        std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64));
    })?;
    Ok(())
}

/// Load and execute a compiled Clean Language WebAssembly module.
pub fn run(wasm_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let wasm_bytes = fs::read(wasm_path)?;

    // Some bridge functions (database, crypto) bridge into async code through
    // the current tokio handle, so execution happens inside a runtime context.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = runtime.enter();

    let engine = Engine::default();
    let module = Module::new(&engine, &wasm_bytes)?;

    let mut linker: Linker<WasmState> = Linker::new(&engine);
    register_all_functions(&mut linker)
        .map_err(|e| format!("Failed to register host bridge functions: {e}"))?;
    register_runner_functions(&mut linker)?;

    let mut store = Store::new(&engine, WasmState::new());
    let instance = linker.instantiate(&mut store, &module)?;

    // The compiler exports __heap_ptr as the first free byte after its data
    // sections; host allocations must start there.
    if let Some(heap_ptr) = instance
        .get_global(&mut store, "__heap_ptr")
        .and_then(|g| g.get(&mut store).i32())
    {
        store.data_mut().memory.set_offset(heap_ptr as usize);
    }

    let entry = ENTRY_POINTS
        .iter()
        .find_map(|name| instance.get_func(&mut store, name));

    match entry {
        Some(func) => {
            func.call(&mut store, &[], &mut [])?;
            Ok(())
        }
        None => {
            let available: Vec<String> = instance
                .exports(&mut store)
                .filter_map(|e| {
                    let name = e.name().to_string();
                    e.into_func().map(|_| name)
                })
                .collect();
            Err(format!(
                "No start/main function found. Available function exports: {:?}",
                available
            )
            .into())
        }
    }
}
