# CLAUDE.md - Clean Runner Development Guide

This file provides guidance when working with the Clean Runner codebase.

## Project Overview

Clean Runner is a standalone WebAssembly runner for Clean Language programs. It uses
`wasmtime` to execute `.wasm` files produced by the Clean Language compiler, providing
a minimal runtime environment for testing and running compiled programs outside of the
full Clean Server stack.

## Important Constraints

- **NEVER** write any reference to AI assistants in any documents, git commits, or any part of the code
- **NEVER** mention AI tools in git commit messages or any part of the codebase

## Cross-Component Work Policy

**CRITICAL: You are a Team Developer AI.** When you discover something in another component, choose the correct channel based on what you found:

| What you found | Channel | Why |
|---|---|---|
| A **bug** (crash, wrong output, spec violation, regression) | **`report_error` MCP tool** — MANDATORY | Fingerprint dedup, occurrence tracking, automatic user notification on fix, visible on errors.cleanlanguage.dev |
| A **design proposal, directive change, schema/API request, architectural ask** | Markdown file in `../foundation/management/cross-component-prompts/` | Requires discussion, not auto-fix |

**Never** write a markdown file for something that is a bug. Bug reports in markdown are invisible to the dashboard, don't notify users when fixed, and can't be queried via `list_component_bugs`.

### What You CAN Do

- Read files from other components to understand interfaces
- Call `report_error` for bugs found in other components
- Write markdown prompts for design/architecture discussions
- Update your component to work with existing interfaces

### What You MUST NOT Do

- Directly edit code in other components
- Make changes to other components' configuration files
- Write a markdown file for something that is a bug — use `report_error` instead

See `../foundation/management/USER_TYPES_AND_ERROR_REPORTING.md` for the full policy.

## Architecture

Clean Runner sits at **Layer 2** of the platform architecture — it implements the host bridge
functions that WASM modules import. It is NOT a replacement for clean-server; it is a
lightweight executor for development and testing.

- Execution layers: `../foundation/spec/platform/EXECUTION_LAYERS.md`
- Host bridge spec: `../foundation/spec/platform/HOST_BRIDGE.md`

## Common Commands

```bash
cargo build
cargo run -- path/to/output.wasm
```
