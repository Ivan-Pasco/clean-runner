#![allow(clippy::uninlined_format_args)]

mod runner;

use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <file.wasm> [args...]", args[0]);
        eprintln!();
        eprintln!("Runs a compiled Clean Language WebAssembly module.");
        eprintln!("The module's start/main function is called automatically.");
        process::exit(1);
    }

    if args[1] == "--version" || args[1] == "-V" {
        println!("clean-runner {}", env!("CARGO_PKG_VERSION"));
        process::exit(0);
    }

    let wasm_path = &args[1];

    match runner::run(wasm_path) {
        Ok(()) => {
            process::exit(0);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    }
}
