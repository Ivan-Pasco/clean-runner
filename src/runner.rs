//! Clean Language WASM host bridge and execution logic.
//!
//! All host functions registered here match the ABI contract defined in
//! `foundation/platform-architecture/HOST_BRIDGE.md`.  String parameters use
//! the length-prefixed format: [4-byte little-endian length][content bytes].
//!
//! This module is the authoritative runner for non-HTTP contexts (scripts, CLI
//! tools, compiler integration tests).  It is NOT a web server.

#![allow(clippy::uninlined_format_args)]
#![allow(deprecated)]

use std::fs;
use std::sync::Mutex;
use wasmtime::{Caller, Engine, Extern, Linker, Memory, Module, Store};

// ---------------------------------------------------------------------------
// Global allocator — arena-style, scope-based memory management
// ---------------------------------------------------------------------------

// Starts at 1 MB — the boundary between compile-time data sections and the
// runtime heap.  Must match `native_stdlib::HEAP_START` (1,048,576) in
// the compiler's `src/codegen/native_stdlib/mod.rs`.
// Memory layout: [0..1KB] reserved, [1KB..1MB] data section, [1MB..] heap.
static NEXT_ALLOCATION_OFFSET: Mutex<usize> = Mutex::new(1_048_576);

// Scope marks for arena-style memory management.
// On scope entry (loop, function) push the current offset.
// On scope exit pop and reset — instant deallocation of all scope allocations.
static SCOPE_MARKS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------------------
// allocate_string_in_memory
// ---------------------------------------------------------------------------

/// Allocate a length-prefixed string in WASM linear memory.
///
/// Format written: `[u32 little-endian length][content bytes]`.
///
/// CRITICAL: Keeps the WASM global `__heap_ptr` (global 0) in sync with the
/// host-side `NEXT_ALLOCATION_OFFSET` so that WASM-side malloc does not return
/// addresses that overlap with host-allocated data.
fn allocate_string_in_memory(
    memory: &Memory,
    caller: &mut Caller<'_, ()>,
    string_value: &str,
) -> i32 {
    let string_bytes = string_value.as_bytes();
    let total_size = 4 + string_bytes.len(); // 4-byte length prefix + content

    // Step 1: Sync host-side offset with WASM __heap_ptr (global 0).
    // The WASM side may have advanced __heap_ptr via its own malloc calls,
    // so take the maximum of both to avoid overlapping allocations.
    let wasm_heap = if let Some(Extern::Global(heap_global)) = caller.get_export("__heap_ptr") {
        heap_global.get(&mut *caller).i32().unwrap_or(0) as usize
    } else {
        0
    };

    let mut next_offset = NEXT_ALLOCATION_OFFSET.lock().unwrap();
    if wasm_heap > *next_offset {
        *next_offset = wasm_heap;
    }

    let offset = *next_offset;
    let aligned_end = offset + ((total_size + 7) & !7);
    *next_offset = aligned_end;
    drop(next_offset);

    // Step 2: Grow linear memory if the allocation would exceed current pages.
    let current_pages = memory.size(&mut *caller) as usize;
    let current_bytes = current_pages * 65_536;
    if aligned_end > current_bytes {
        let needed_pages = (aligned_end - current_bytes).div_ceil(65_536) as u64;
        if memory.grow(&mut *caller, needed_pages).is_err() {
            println!("WARNING: memory.grow failed for {needed_pages} pages");
            return 0;
        }
    }

    // Step 3: Write string data.
    let data = memory.data_mut(&mut *caller);
    if offset + total_size > data.len() {
        println!(
            "WARNING: Not enough WASM memory for string allocation. \
             Offset: {offset}, Size: {total_size}, Memory: {memory_len}",
            memory_len = data.len()
        );
        return 0;
    }

    // Length (4 bytes, little-endian) followed by content.
    data[offset..offset + 4].copy_from_slice(&(string_bytes.len() as u32).to_le_bytes());
    data[offset + 4..offset + 4 + string_bytes.len()].copy_from_slice(string_bytes);

    // Step 4: Update WASM global __heap_ptr so WASM-side malloc starts past
    // this allocation.
    if let Some(Extern::Global(heap_global)) = caller.get_export("__heap_ptr") {
        heap_global
            .set(&mut *caller, wasmtime::Val::I32(aligned_end as i32))
            .ok();
    }

    offset as i32
}

// ---------------------------------------------------------------------------
// Helper: read a length-prefixed string from WASM memory
// ---------------------------------------------------------------------------

fn read_length_prefixed_string(data: &[u8], ptr: usize) -> Option<String> {
    if ptr + 4 > data.len() {
        return None;
    }
    let len =
        u32::from_le_bytes([data[ptr], data[ptr + 1], data[ptr + 2], data[ptr + 3]]) as usize;
    if ptr + 4 + len > data.len() {
        return None;
    }
    std::str::from_utf8(&data[ptr + 4..ptr + 4 + len])
        .ok()
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// pub fn run — entry point
// ---------------------------------------------------------------------------

/// Load and execute a compiled Clean Language WebAssembly module.
///
/// All host bridge functions are registered before instantiation.
/// The module's `start`, `_start`, `main`, or `_main` export is called.
pub fn run(wasm_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("Loading WebAssembly file: {wasm_path}");

    let wasm_bytes = fs::read(wasm_path)?;
    println!("File size: {len} bytes", len = wasm_bytes.len());

    // Use a plain synchronous Engine — no async, no fuel, no epoch interruption.
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());

    let module = Module::new(&engine, &wasm_bytes)?;

    let mut linker: Linker<()> = Linker::new(&engine);

    // -----------------------------------------------------------------------
    // Console I/O
    // -----------------------------------------------------------------------

    // print(ptr: i32, len: i32) — output text without newline
    linker.func_wrap(
        "env",
        "print",
        |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
            let mem = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                print!("[print: ptr={ptr}, len={len}]");
                return;
            };

            let data = if let Some(data) = mem.data(&caller).get(ptr as usize..(ptr + len) as usize)
            {
                data
            } else {
                print!("[print: invalid range ptr={}, len={}]", ptr, len);
                return;
            };

            match std::str::from_utf8(data) {
                Ok(s) => print!("{}", s),
                Err(_) => print!("[invalid utf8: {} bytes]", len),
            }
        },
    )?;

    // printl(ptr: i32, len: i32) — output text with newline
    linker.func_wrap(
        "env",
        "printl",
        |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
            let mem = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                println!("[printl: ptr={ptr}, len={len}]");
                return;
            };

            let data = if let Some(data) = mem.data(&caller).get(ptr as usize..(ptr + len) as usize)
            {
                data
            } else {
                println!("[printl: invalid range ptr={ptr}, len={len}]");
                return;
            };

            match std::str::from_utf8(data) {
                Ok(s) => println!("{s}"),
                Err(_) => println!("[invalid utf8: {len} bytes]"),
            }
        },
    )?;

    // print_simple(value: i32) — print integer without newline
    linker.func_wrap("env", "print_simple", |value: i32| {
        print!("{}", value);
    })?;

    // printl_simple(value: i32) — print integer with newline
    linker.func_wrap("env", "printl_simple", |value: i32| {
        println!("{value}");
    })?;

    // print_integer(value: i64) — typed integer print
    linker.func_wrap("env", "print_integer", |value: i64| {
        println!("{value}");
    })?;

    // print_float(value: f64) — typed float print
    linker.func_wrap("env", "print_float", |value: f64| {
        println!("{value}");
    })?;

    // print_boolean(value: i32) — typed boolean print ("true" / "false")
    linker.func_wrap("env", "print_boolean", |value: i32| {
        println!("{}", if value != 0 { "true" } else { "false" });
    })?;

    // -----------------------------------------------------------------------
    // File I/O — ABI stubs for non-HTTP contexts
    // -----------------------------------------------------------------------

    linker.func_wrap(
        "env",
        "file_write",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "file_read", |_: i32, _: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "file_exists", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "file_delete", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "file_append",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;

    // -----------------------------------------------------------------------
    // HTTP client — ABI stubs
    // -----------------------------------------------------------------------

    linker.func_wrap("env", "http_get", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "http_post",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "http_put", |_: i32, _: i32, _: i32, _: i32| -> i32 {
        0
    })?;
    linker.func_wrap("env", "http_delete", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "http_patch",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "http_head", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "http_options", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "http_get_with_headers",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "http_post_with_headers",
        |_: i32, _: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "http_post_json",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "http_put_json",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "http_patch_json",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap(
        "env",
        "http_post_form",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "http_set_user_agent", |_: i32, _: i32| {})?;
    linker.func_wrap("env", "http_set_timeout", |_: i32| {})?;
    linker.func_wrap("env", "http_set_max_redirects", |_: i32| {})?;
    linker.func_wrap("env", "http_enable_cookies", |_: i32| {})?;
    linker.func_wrap("env", "http_get_response_code", || -> i32 { 0 })?;
    linker.func_wrap("env", "http_get_response_headers", || -> i32 { 0 })?;
    linker.func_wrap("env", "http_encode_url", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "http_decode_url", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "http_build_query", |_: i32, _: i32| -> i32 { 0 })?;

    // -----------------------------------------------------------------------
    // HTTP server — ABI stubs (kept for ABI compatibility with server-compiled modules)
    // -----------------------------------------------------------------------

    linker.func_wrap(
        "env",
        "_http_route",
        |_: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    linker.func_wrap("env", "_http_listen", |_: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "_req_param", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "_req_query", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "_req_body", || -> i32 { 0 })?;
    linker.func_wrap("env", "_req_header", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "_req_method", || -> i32 { 0 })?;
    linker.func_wrap("env", "_req_path", || -> i32 { 0 })?;
    linker.func_wrap("env", "_req_cookie", |_: i32, _: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "_http_route_protected",
        |_: i32, _: i32, _: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;

    // Session stubs
    // _session_store(user_id: i32, rolePtr: i32, roleLen: i32, claimsPtr: i32, claimsLen: i32) -> i32
    linker.func_wrap(
        "env",
        "_session_store",
        |_: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;
    // _session_get() -> i32
    linker.func_wrap("env", "_session_get", || -> i32 { 0 })?;
    // _session_delete() -> i32
    linker.func_wrap("env", "_session_delete", || -> i32 { 0 })?;
    // _http_set_cookie(cookiePtr: i32, cookieLen: i32) -> i32
    linker.func_wrap("env", "_http_set_cookie", |_: i32, _: i32| -> i32 { 0 })?;

    // Auth stubs
    // _auth_get_session() -> i32
    linker.func_wrap("env", "_auth_get_session", || -> i32 { 0 })?;
    // _auth_require_auth() -> i32
    linker.func_wrap("env", "_auth_require_auth", || -> i32 { 0 })?;
    // _auth_require_role(rolePtr: i32, roleLen: i32) -> i32
    linker.func_wrap("env", "_auth_require_role", |_: i32, _: i32| -> i32 { 0 })?;
    // _auth_can(permPtr: i32, permLen: i32) -> i32
    linker.func_wrap("env", "_auth_can", |_: i32, _: i32| -> i32 { 0 })?;
    // _auth_has_any_role(rolesPtr: i32, rolesLen: i32) -> i32
    linker.func_wrap("env", "_auth_has_any_role", |_: i32, _: i32| -> i32 { 0 })?;

    // -----------------------------------------------------------------------
    // Input — stubs (stdin is not supported in non-interactive contexts)
    // -----------------------------------------------------------------------

    linker.func_wrap("env", "input", |_: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "input_integer", |_: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "input_float", |_: i32| -> f64 { 0.0 })?;
    linker.func_wrap("env", "input_yesno", |_: i32| -> i32 { 0 })?;
    linker.func_wrap(
        "env",
        "input_range",
        |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 },
    )?;

    // -----------------------------------------------------------------------
    // Math functions — full implementations (both dot and underscore naming)
    // -----------------------------------------------------------------------

    linker.func_wrap("env", "math_pow", |base: f64, exp: f64| -> f64 {
        base.powf(exp)
    })?;
    linker.func_wrap("env", "math.pow", |base: f64, exp: f64| -> f64 {
        base.powf(exp)
    })?;
    linker.func_wrap("env", "math_sin", |x: f64| -> f64 { x.sin() })?;
    linker.func_wrap("env", "math.sin", |x: f64| -> f64 { x.sin() })?;
    linker.func_wrap("env", "math_cos", |x: f64| -> f64 { x.cos() })?;
    linker.func_wrap("env", "math.cos", |x: f64| -> f64 { x.cos() })?;
    linker.func_wrap("env", "math_tan", |x: f64| -> f64 { x.tan() })?;
    linker.func_wrap("env", "math.tan", |x: f64| -> f64 { x.tan() })?;
    linker.func_wrap("env", "math_asin", |x: f64| -> f64 { x.asin() })?;
    linker.func_wrap("env", "math.asin", |x: f64| -> f64 { x.asin() })?;
    linker.func_wrap("env", "math_acos", |x: f64| -> f64 { x.acos() })?;
    linker.func_wrap("env", "math.acos", |x: f64| -> f64 { x.acos() })?;
    linker.func_wrap("env", "math_atan", |x: f64| -> f64 { x.atan() })?;
    linker.func_wrap("env", "math.atan", |x: f64| -> f64 { x.atan() })?;
    linker.func_wrap("env", "math_atan2", |y: f64, x: f64| -> f64 { y.atan2(x) })?;
    linker.func_wrap("env", "math.atan2", |y: f64, x: f64| -> f64 { y.atan2(x) })?;
    linker.func_wrap("env", "math_sinh", |x: f64| -> f64 { x.sinh() })?;
    linker.func_wrap("env", "math.sinh", |x: f64| -> f64 { x.sinh() })?;
    linker.func_wrap("env", "math_cosh", |x: f64| -> f64 { x.cosh() })?;
    linker.func_wrap("env", "math.cosh", |x: f64| -> f64 { x.cosh() })?;
    linker.func_wrap("env", "math_tanh", |x: f64| -> f64 { x.tanh() })?;
    linker.func_wrap("env", "math.tanh", |x: f64| -> f64 { x.tanh() })?;
    linker.func_wrap("env", "math_ln", |x: f64| -> f64 { x.ln() })?;
    linker.func_wrap("env", "math.ln", |x: f64| -> f64 { x.ln() })?;
    linker.func_wrap("env", "math_log10", |x: f64| -> f64 { x.log10() })?;
    linker.func_wrap("env", "math.log10", |x: f64| -> f64 { x.log10() })?;
    linker.func_wrap("env", "math_log2", |x: f64| -> f64 { x.log2() })?;
    linker.func_wrap("env", "math.log2", |x: f64| -> f64 { x.log2() })?;
    linker.func_wrap("env", "math_exp", |x: f64| -> f64 { x.exp() })?;
    linker.func_wrap("env", "math.exp", |x: f64| -> f64 { x.exp() })?;
    linker.func_wrap("env", "math_exp2", |x: f64| -> f64 { x.exp2() })?;
    linker.func_wrap("env", "math.exp2", |x: f64| -> f64 { x.exp2() })?;
    linker.func_wrap("env", "math_sqrt", |x: f64| -> f64 { x.sqrt() })?;
    linker.func_wrap("env", "math.sqrt", |x: f64| -> f64 { x.sqrt() })?;
    linker.func_wrap("env", "math_floor", |x: f64| -> f64 { x.floor() })?;
    linker.func_wrap("env", "math.floor", |x: f64| -> f64 { x.floor() })?;
    linker.func_wrap("env", "math_ceil", |x: f64| -> f64 { x.ceil() })?;
    linker.func_wrap("env", "math.ceil", |x: f64| -> f64 { x.ceil() })?;
    linker.func_wrap("env", "math_round", |x: f64| -> f64 { x.round() })?;
    linker.func_wrap("env", "math.round", |x: f64| -> f64 { x.round() })?;
    linker.func_wrap("env", "math_abs", |x: f64| -> f64 { x.abs() })?;
    linker.func_wrap("env", "math.abs", |x: f64| -> f64 { x.abs() })?;

    // -----------------------------------------------------------------------
    // Conditional selection — full implementations
    // -----------------------------------------------------------------------

    linker.func_wrap(
        "env",
        "conditional_integer",
        |condition: i32, true_value: i32, false_value: i32| -> i32 {
            if condition != 0 {
                true_value
            } else {
                false_value
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "conditional_number",
        |condition: i32, true_value: f64, false_value: f64| -> f64 {
            if condition != 0 {
                true_value
            } else {
                false_value
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "conditional_boolean",
        |condition: i32, true_value: i32, false_value: i32| -> i32 {
            if condition != 0 {
                true_value
            } else {
                false_value
            }
        },
    )?;

    linker.func_wrap(
        "env",
        "conditional_string",
        |condition: i32, true_value: i32, false_value: i32| -> i32 {
            if condition != 0 {
                true_value
            } else {
                false_value
            }
        },
    )?;

    // -----------------------------------------------------------------------
    // Type conversions — full implementations
    // -----------------------------------------------------------------------

    // int_to_string(value: i32) -> i32 (ptr to length-prefixed string)
    linker.func_wrap(
        "env",
        "int_to_string",
        |mut caller: Caller<'_, ()>, value: i32| -> i32 {
            let string_value = value.to_string();
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &string_value);
                }
            }
            0
        },
    )?;

    // float_to_string(value: f64) -> i32 (ptr to length-prefixed string)
    linker.func_wrap(
        "env",
        "float_to_string",
        |mut caller: Caller<'_, ()>, value: f64| -> i32 {
            let string_value = value.to_string();
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &string_value);
                }
            }
            0
        },
    )?;

    // bool_to_string(value: i32) -> i32 (ptr to length-prefixed string)
    linker.func_wrap(
        "env",
        "bool_to_string",
        |mut caller: Caller<'_, ()>, value: i32| -> i32 {
            let string_value = if value != 0 { "true" } else { "false" };
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, string_value);
                }
            }
            0
        },
    )?;

    linker.func_wrap("env", "string_to_int", |_: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "string_to_float", |_: i32| -> f64 { 0.0 })?;

    // -----------------------------------------------------------------------
    // String operations
    // -----------------------------------------------------------------------

    // string.concat(ptr1: i32, ptr2: i32) -> i32
    // Each pointer is a length-prefixed string.  Returns ptr to concatenated result.
    linker.func_wrap(
        "env",
        "string.concat",
        |mut caller: Caller<'_, ()>, ptr1: i32, ptr2: i32| -> i32 {
            let memory = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                eprintln!("string.concat: Failed to get memory");
                return 0;
            };

            let (str1, str2) = {
                let data = memory.data(&caller);
                let s1 = match read_length_prefixed_string(data, ptr1 as usize) {
                    Some(s) => s,
                    None => {
                        eprintln!("string.concat: ptr1 out of bounds or invalid UTF-8");
                        return 0;
                    }
                };
                let s2 = match read_length_prefixed_string(data, ptr2 as usize) {
                    Some(s) => s,
                    None => {
                        eprintln!("string.concat: ptr2 out of bounds or invalid UTF-8");
                        return 0;
                    }
                };
                (s1, s2)
            };

            let result = str1 + &str2;
            allocate_string_in_memory(&memory, &mut caller, &result)
        },
    )?;

    // string_concat — underscore-named alias (same ABI, same implementation)
    linker.func_wrap(
        "env",
        "string_concat",
        |mut caller: Caller<'_, ()>, ptr1: i32, ptr2: i32| -> i32 {
            let memory = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                eprintln!("string_concat: Failed to get memory");
                return 0;
            };

            let (str1, str2) = {
                let data = memory.data(&caller);
                let s1 = match read_length_prefixed_string(data, ptr1 as usize) {
                    Some(s) => s,
                    None => {
                        eprintln!("string_concat: ptr1 out of bounds or invalid UTF-8");
                        return 0;
                    }
                };
                let s2 = match read_length_prefixed_string(data, ptr2 as usize) {
                    Some(s) => s,
                    None => {
                        eprintln!("string_concat: ptr2 out of bounds or invalid UTF-8");
                        return 0;
                    }
                };
                (s1, s2)
            };

            let result = str1 + &str2;
            allocate_string_in_memory(&memory, &mut caller, &result)
        },
    )?;

    // string_compare(ptr1: i32, ptr2: i32) -> i32
    // Returns 0 if equal, 1 if not equal (C/strcmp convention; codegen uses i32.eqz).
    linker.func_wrap(
        "env",
        "string_compare",
        |mut caller: Caller<'_, ()>, ptr1: i32, ptr2: i32| -> i32 {
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    let data = memory.data(&caller);
                    let s1 = read_length_prefixed_string(data, ptr1 as usize).unwrap_or_default();
                    let s2 = read_length_prefixed_string(data, ptr2 as usize).unwrap_or_default();
                    return if s1 == s2 { 0 } else { 1 };
                }
            }
            0
        },
    )?;

    // string_replace(string_ptr: i32, search_ptr: i32, replace_ptr: i32) -> i32
    // Replaces all occurrences of search with replace in the source string.
    linker.func_wrap(
        "env",
        "string_replace",
        |mut caller: Caller<'_, ()>, string_ptr: i32, search_ptr: i32, replace_ptr: i32| -> i32 {
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    let (source, search, replace) = {
                        let data = memory.data(&caller);
                        let src = read_length_prefixed_string(data, string_ptr as usize)
                            .unwrap_or_default();
                        let srch = read_length_prefixed_string(data, search_ptr as usize)
                            .unwrap_or_default();
                        let repl = read_length_prefixed_string(data, replace_ptr as usize)
                            .unwrap_or_default();
                        (src, srch, repl)
                    };
                    let result = source.replace(&search, &replace);
                    return allocate_string_in_memory(&memory, &mut caller, &result);
                }
            }
            0
        },
    )?;

    // string_trim(ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_trim",
        |mut caller: Caller<'_, ()>, ptr: i32| -> i32 {
            let trimmed = {
                if let Some(memory) = caller.get_export("memory") {
                    if let Some(memory) = memory.into_memory() {
                        let data = memory.data(&caller);
                        match read_length_prefixed_string(data, ptr as usize) {
                            Some(s) => s.trim().to_string(),
                            None => return 0,
                        }
                    } else {
                        return 0;
                    }
                } else {
                    return 0;
                }
            };
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &trimmed);
                }
            }
            0
        },
    )?;

    // string_trim_start(ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_trim_start",
        |mut caller: Caller<'_, ()>, ptr: i32| -> i32 {
            let trimmed = {
                if let Some(memory) = caller.get_export("memory") {
                    if let Some(memory) = memory.into_memory() {
                        let data = memory.data(&caller);
                        match read_length_prefixed_string(data, ptr as usize) {
                            Some(s) => s.trim_start().to_string(),
                            None => return 0,
                        }
                    } else {
                        return 0;
                    }
                } else {
                    return 0;
                }
            };
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &trimmed);
                }
            }
            0
        },
    )?;

    // string_trim_end(ptr: i32) -> i32
    linker.func_wrap(
        "env",
        "string_trim_end",
        |mut caller: Caller<'_, ()>, ptr: i32| -> i32 {
            let trimmed = {
                if let Some(memory) = caller.get_export("memory") {
                    if let Some(memory) = memory.into_memory() {
                        let data = memory.data(&caller);
                        match read_length_prefixed_string(data, ptr as usize) {
                            Some(s) => s.trim_end().to_string(),
                            None => return 0,
                        }
                    } else {
                        return 0;
                    }
                } else {
                    return 0;
                }
            };
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &trimmed);
                }
            }
            0
        },
    )?;

    // string_split(string_ptr: i32, delimiter_ptr: i32) -> i32
    // Returns a list header pointer.  List layout:
    //   [size(4)][capacity(4)][type_id(4)][padding(4)][ptr_0(4)]...[ptr_N(4)]
    // Each element pointer points to a length-prefixed string.
    linker.func_wrap(
        "env",
        "string_split",
        |mut caller: Caller<'_, ()>, string_ptr: i32, delimiter_ptr: i32| -> i32 {
            let memory = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                eprintln!("string_split: no memory");
                return 0;
            };

            let (string_content, delimiter) = {
                let data = memory.data(&caller);
                let sc = match read_length_prefixed_string(data, string_ptr as usize) {
                    Some(s) => s,
                    None => return 0,
                };
                let dl = match read_length_prefixed_string(data, delimiter_ptr as usize) {
                    Some(s) => s,
                    None => return 0,
                };
                (sc, dl)
            };

            let parts: Vec<String> = string_content
                .split(delimiter.as_str())
                .map(|s| s.to_string())
                .collect();
            let num_parts = parts.len();

            // Layout: 16-byte header + num_parts * 4-byte pointers
            let list_size = 16 + num_parts * 4;

            // Sync host offset with WASM __heap_ptr
            let current_heap =
                if let Some(Extern::Global(heap_global)) = caller.get_export("__heap_ptr") {
                    heap_global.get(&mut caller).i32().unwrap_or(0) as usize
                } else {
                    0
                };

            let mut next_offset_guard = NEXT_ALLOCATION_OFFSET.lock().unwrap();
            let sync_heap = (*next_offset_guard).max(current_heap);

            let list_ptr = sync_heap;
            let mut next_ptr = list_ptr + list_size;

            // Reserve space for each length-prefixed string
            let mut string_ptrs: Vec<u32> = Vec::with_capacity(num_parts);
            for part in &parts {
                string_ptrs.push(next_ptr as u32);
                next_ptr += 4 + part.len(); // 4-byte length prefix + content
            }

            let new_heap_ptr = ((next_ptr + 7) & !7) as u32;
            *next_offset_guard = new_heap_ptr as usize;
            drop(next_offset_guard);

            // Grow memory if needed
            let current_bytes = memory.size(&caller) as usize * 65_536;
            if (new_heap_ptr as usize) > current_bytes {
                let needed_pages =
                    (new_heap_ptr as usize - current_bytes).div_ceil(65_536) as u64;
                let _ = memory.grow(&mut caller, needed_pages);
            }

            // Update WASM global __heap_ptr
            if let Some(Extern::Global(heap_global)) = caller.get_export("__heap_ptr") {
                heap_global
                    .set(&mut caller, wasmtime::Val::I32(new_heap_ptr as i32))
                    .ok();
            }

            let data_mut = memory.data_mut(&mut caller);

            // Write list header: [size][capacity][type_id=3 (string)][padding]
            data_mut[list_ptr..list_ptr + 4]
                .copy_from_slice(&(num_parts as u32).to_le_bytes());
            data_mut[list_ptr + 4..list_ptr + 8]
                .copy_from_slice(&(num_parts as u32).to_le_bytes());
            data_mut[list_ptr + 8..list_ptr + 12].copy_from_slice(&3u32.to_le_bytes());
            data_mut[list_ptr + 12..list_ptr + 16].copy_from_slice(&0u32.to_le_bytes());

            // Write element pointers
            for (i, &ptr) in string_ptrs.iter().enumerate() {
                let offset = list_ptr + 16 + i * 4;
                data_mut[offset..offset + 4].copy_from_slice(&ptr.to_le_bytes());
            }

            // Write string contents (length-prefixed)
            for (i, part) in parts.iter().enumerate() {
                let ptr = string_ptrs[i] as usize;
                let part_bytes = part.as_bytes();
                data_mut[ptr..ptr + 4]
                    .copy_from_slice(&(part_bytes.len() as u32).to_le_bytes());
                data_mut[ptr + 4..ptr + 4 + part_bytes.len()].copy_from_slice(part_bytes);
            }

            list_ptr as i32
        },
    )?;

    // string.split — dot-notation alias (same implementation, synced heap logic)
    linker.func_wrap(
        "env",
        "string.split",
        |mut caller: Caller<'_, ()>, string_ptr: i32, delimiter_ptr: i32| -> i32 {
            let memory = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                eprintln!("string.split: no memory");
                return 0;
            };

            let (string_content, delimiter) = {
                let data = memory.data(&caller);
                let sc = match read_length_prefixed_string(data, string_ptr as usize) {
                    Some(s) => s,
                    None => return 0,
                };
                let dl = match read_length_prefixed_string(data, delimiter_ptr as usize) {
                    Some(s) => s,
                    None => return 0,
                };
                (sc, dl)
            };

            let parts: Vec<String> = string_content
                .split(delimiter.as_str())
                .map(|s| s.to_string())
                .collect();
            let num_parts = parts.len();

            let list_size = 16 + num_parts * 4;

            let current_heap =
                if let Some(Extern::Global(heap_global)) = caller.get_export("__heap_ptr") {
                    heap_global.get(&mut caller).i32().unwrap_or(0) as usize
                } else {
                    0
                };

            let mut next_offset_guard = NEXT_ALLOCATION_OFFSET.lock().unwrap();
            let sync_heap = (*next_offset_guard).max(current_heap);

            let list_ptr = sync_heap;
            let mut next_ptr = list_ptr + list_size;

            let mut string_ptrs: Vec<u32> = Vec::with_capacity(num_parts);
            for part in &parts {
                string_ptrs.push(next_ptr as u32);
                next_ptr += 4 + part.len();
            }

            let new_heap_ptr = ((next_ptr + 7) & !7) as u32;
            *next_offset_guard = new_heap_ptr as usize;
            drop(next_offset_guard);

            let current_bytes = memory.size(&caller) as usize * 65_536;
            if (new_heap_ptr as usize) > current_bytes {
                let needed_pages =
                    (new_heap_ptr as usize - current_bytes).div_ceil(65_536) as u64;
                let _ = memory.grow(&mut caller, needed_pages);
            }

            if let Some(Extern::Global(heap_global)) = caller.get_export("__heap_ptr") {
                heap_global
                    .set(&mut caller, wasmtime::Val::I32(new_heap_ptr as i32))
                    .ok();
            }

            let data_mut = memory.data_mut(&mut caller);

            data_mut[list_ptr..list_ptr + 4]
                .copy_from_slice(&(num_parts as u32).to_le_bytes());
            data_mut[list_ptr + 4..list_ptr + 8]
                .copy_from_slice(&(num_parts as u32).to_le_bytes());
            data_mut[list_ptr + 8..list_ptr + 12].copy_from_slice(&3u32.to_le_bytes());
            data_mut[list_ptr + 12..list_ptr + 16].copy_from_slice(&0u32.to_le_bytes());

            for (i, &ptr) in string_ptrs.iter().enumerate() {
                let offset = list_ptr + 16 + i * 4;
                data_mut[offset..offset + 4].copy_from_slice(&ptr.to_le_bytes());
            }

            for (i, part) in parts.iter().enumerate() {
                let ptr = string_ptrs[i] as usize;
                let part_bytes = part.as_bytes();
                data_mut[ptr..ptr + 4]
                    .copy_from_slice(&(part_bytes.len() as u32).to_le_bytes());
                data_mut[ptr + 4..ptr + 4 + part_bytes.len()].copy_from_slice(part_bytes);
            }

            list_ptr as i32
        },
    )?;

    // string.toUpperCase — stub
    linker.func_wrap("env", "string.toUpperCase", |_: i32| -> i32 { 0 })?;
    // string.toLowerCase — stub
    linker.func_wrap("env", "string.toLowerCase", |_: i32| -> i32 { 0 })?;

    // string_repeat(str_ptr: i32, str_len: i32, count: i32) -> i32
    linker.func_wrap(
        "env",
        "string_repeat",
        |mut caller: Caller<'_, ()>, ptr: i32, _len: i32, count: i32| -> i32 {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let data = mem.data(&caller);
            let s = match read_length_prefixed_string(data, ptr as usize) {
                Some(s) => s,
                None => return 0,
            };
            let result = s.repeat(count.max(0) as usize);
            allocate_string_in_memory(&mem, &mut caller, &result)
        },
    )?;

    // string_matches(str_ptr: i32, str_len: i32, pattern_ptr: i32, pattern_len: i32) -> i32
    linker.func_wrap(
        "env",
        "string_matches",
        |mut caller: Caller<'_, ()>, str_ptr: i32, _str_len: i32, pat_ptr: i32, _pat_len: i32| -> i32 {
            let mem = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return 0,
            };
            let data = mem.data(&caller);
            let s = read_length_prefixed_string(data, str_ptr as usize).unwrap_or_default();
            let pattern = read_length_prefixed_string(data, pat_ptr as usize).unwrap_or_default();
            let matches = match pattern.as_str() {
                "email" => s.contains('@') && s.contains('.'),
                "url" => s.starts_with("http://") || s.starts_with("https://"),
                "uuid" => s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4,
                _ => false,
            };
            if matches { 1 } else { 0 }
        },
    )?;

    // _server_sleep(ms: i64) — blocks the current thread
    linker.func_wrap("env", "_server_sleep", |ms: i64| {
        std::thread::sleep(std::time::Duration::from_millis(ms.max(0) as u64));
    })?;

    // -----------------------------------------------------------------------
    // Memory management (memory_runtime namespace)
    // -----------------------------------------------------------------------

    // mem_alloc(type_id: i32, size: i32) -> i32
    linker.func_wrap(
        "memory_runtime",
        "mem_alloc",
        |_type_id: i32, size: i32| -> i32 {
            let mut next_offset = NEXT_ALLOCATION_OFFSET.lock().unwrap();
            let offset = *next_offset;
            *next_offset += ((size as usize) + 7) & !7;
            offset as i32
        },
    )?;

    // mem_retain(ptr: i32) — no-op (reference counting not used in runner)
    linker.func_wrap("memory_runtime", "mem_retain", |_ptr: i32| {})?;

    // mem_release(ptr: i32) — no-op (scope-based release is used instead)
    linker.func_wrap("memory_runtime", "mem_release", |_ptr: i32| {})?;

    // mem_scope_push — save current allocation offset for later reset
    linker.func_wrap("memory_runtime", "mem_scope_push", || {
        let next_offset = NEXT_ALLOCATION_OFFSET.lock().unwrap();
        let mut scope_marks = SCOPE_MARKS.lock().unwrap();
        scope_marks.push(*next_offset);
    })?;

    // mem_scope_pop — reset allocation offset to last saved mark
    linker.func_wrap("memory_runtime", "mem_scope_pop", || {
        let mut scope_marks = SCOPE_MARKS.lock().unwrap();
        if let Some(mark) = scope_marks.pop() {
            let mut next_offset = NEXT_ALLOCATION_OFFSET.lock().unwrap();
            *next_offset = mark;
        }
    })?;

    // -----------------------------------------------------------------------
    // Method-style conversions (type.method naming)
    // -----------------------------------------------------------------------

    // integer.*
    linker.func_wrap(
        "env",
        "integer.toString",
        |mut caller: Caller<'_, ()>, value: i32| -> i32 {
            let string_value = value.to_string();
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &string_value);
                }
            }
            0
        },
    )?;
    linker.func_wrap("env", "integer.toInteger", |value: i32| -> i32 { value })?;
    linker.func_wrap("env", "integer.toNumber", |value: i32| -> f64 {
        f64::from(value)
    })?;
    linker.func_wrap("env", "integer.toBoolean", |value: i32| -> i32 {
        i32::from(value != 0)
    })?;
    linker.func_wrap("env", "integer.length", |_: i32| -> i32 { 0 })?;

    // number.*
    linker.func_wrap(
        "env",
        "number.toString",
        |mut caller: Caller<'_, ()>, value: f64| -> i32 {
            let string_value = value.to_string();
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, &string_value);
                }
            }
            0
        },
    )?;
    linker.func_wrap("env", "number.toInteger", |value: f64| -> i32 {
        value as i32
    })?;
    linker.func_wrap("env", "number.toNumber", |value: f64| -> f64 { value })?;
    linker.func_wrap("env", "number.toBoolean", |value: f64| -> i32 {
        i32::from(value != 0.0)
    })?;
    linker.func_wrap("env", "number.length", |_: f64| -> i32 { 0 })?;

    // string.*
    linker.func_wrap("env", "string.toString", |value: i32| -> i32 { value })?;
    linker.func_wrap("env", "string.toInteger", |_: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "string.toNumber", |_: i32| -> f64 { 0.0 })?;
    linker.func_wrap("env", "string.toBoolean", |_: i32| -> i32 { 0 })?;
    linker.func_wrap("env", "string.length", |_: i32| -> i32 { 0 })?;

    // boolean.*
    linker.func_wrap(
        "env",
        "boolean.toString",
        |mut caller: Caller<'_, ()>, value: i32| -> i32 {
            let string_value = if value != 0 { "true" } else { "false" };
            if let Some(memory) = caller.get_export("memory") {
                if let Some(memory) = memory.into_memory() {
                    return allocate_string_in_memory(&memory, &mut caller, string_value);
                }
            }
            0
        },
    )?;
    linker.func_wrap("env", "boolean.toInteger", |value: i32| -> i32 { value })?;
    linker.func_wrap("env", "boolean.toNumber", |value: i32| -> f64 {
        f64::from(value)
    })?;
    linker.func_wrap("env", "boolean.toBoolean", |value: i32| -> i32 { value })?;
    linker.func_wrap("env", "boolean.length", |_: i32| -> i32 { 0 })?;

    // -----------------------------------------------------------------------
    // Array / List operations
    // -----------------------------------------------------------------------

    // array_get(array_ptr: i32, index: i32) -> i32
    // Array memory layout: [ref_count(4)][type_id(4)][size(4)][gc_flags(1)][length(4)][data...]
    linker.func_wrap(
        "env",
        "array_get",
        |mut caller: Caller<'_, ()>, array_ptr: i32, index: i32| -> i32 {
            let memory = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                println!("[array_get: no memory export]");
                return 0;
            };

            let data = memory.data(&caller);
            let ptr_usize = array_ptr as usize;

            if ptr_usize + 17 > data.len() {
                println!("[array_get: invalid array pointer {}]", array_ptr);
                return 0;
            }

            // Length is at byte offset 13 (after ref_count(4) + type_id(4) + size(4) + gc_flags(1))
            let length_offset = ptr_usize + 13;
            let data_offset = ptr_usize + 17;

            let length = u32::from_le_bytes([
                data[length_offset],
                data[length_offset + 1],
                data[length_offset + 2],
                data[length_offset + 3],
            ]) as i32;

            if index < 0 || index >= length {
                println!(
                    "[array_get: index {} out of bounds for array of length {}]",
                    index, length
                );
                return 0;
            }

            let element_offset = data_offset + (index as usize * 4);
            if element_offset + 4 > data.len() {
                println!("[array_get: element access out of memory bounds]");
                return 0;
            }

            i32::from_le_bytes([
                data[element_offset],
                data[element_offset + 1],
                data[element_offset + 2],
                data[element_offset + 3],
            ])
        },
    )?;

    // list.push_f64(array_ptr: i32, value: f64) -> i32
    // List layout: [length(4)][capacity(4)][type_id(4)][flags(4)][data...]
    // Each f64 element is 8 bytes.  Returns the (same) array pointer.
    linker.func_wrap(
        "env",
        "list.push_f64",
        |mut caller: Caller<'_, ()>, array_ptr: i32, value: f64| -> i32 {
            let memory = if let Some(Extern::Memory(mem)) = caller.get_export("memory") {
                mem
            } else {
                eprintln!("[list.push_f64: no memory export]");
                return array_ptr;
            };

            let (length, element_offset) = {
                let data = memory.data(&caller);
                let ptr = array_ptr as usize;
                if ptr + 16 > data.len() {
                    eprintln!("[list.push_f64: invalid pointer {}]", array_ptr);
                    return array_ptr;
                }
                let length =
                    u32::from_le_bytes([data[ptr], data[ptr + 1], data[ptr + 2], data[ptr + 3]])
                        as usize;
                let data_offset = ptr + 16;
                let element_offset = data_offset + length * 8;
                (length, element_offset)
            };

            if element_offset + 8 > memory.data(&caller).len() {
                eprintln!("[list.push_f64: out of memory bounds]");
                return array_ptr;
            }

            let data_mut = memory.data_mut(&mut caller);
            let bytes = value.to_le_bytes();
            data_mut[element_offset..element_offset + 8].copy_from_slice(&bytes);

            let new_length = (length + 1) as u32;
            let ptr = array_ptr as usize;
            data_mut[ptr..ptr + 4].copy_from_slice(&new_length.to_le_bytes());

            array_ptr
        },
    )?;

    // -----------------------------------------------------------------------
    // Instantiate module and call start function
    // -----------------------------------------------------------------------

    let instance = linker.instantiate(&mut store, &module)?;

    // Try start, _start, main, _main — in that order.
    let start_func = instance
        .get_func(&mut store, "start")
        .or_else(|| instance.get_func(&mut store, "_start"))
        .or_else(|| instance.get_func(&mut store, "main"))
        .or_else(|| instance.get_func(&mut store, "_main"));

    if let Some(func) = start_func {
        func.call(&mut store, &[], &mut [])?;
    } else {
        // List available function exports for diagnostics.
        let available: Vec<String> = instance
            .exports(&mut store)
            .filter_map(|e| {
                let name = e.name().to_string();
                if e.into_func().is_some() {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        return Err(format!(
            "No start/main function found. Available function exports: {:?}",
            available
        )
        .into());
    }

    Ok(())
}
