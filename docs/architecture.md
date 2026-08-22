# Architecture and design contract

This document describes the architectural design, subsystem boundaries, and execution contracts of Oxvim.

## Overview

Oxvim is structured as an embedded server and client pair communicating over MessagePack-RPC. The editor core executes on a single main thread, preserving the execution invariants and concurrency model expected by Neovim Lua plugins and external RPC clients.

The repository is divided into discrete crates with explicit dependency boundaries. Core data structures (`ox-types`, `ox-text`, `ox-editor`) do not depend on Lua runtimes or user interface implementations. The Lua host (`ox-lua`) and UI layers (`ox-ui`, `ox-tui`) interact with the editor core through typed APIs and structured event streams.

## Single-writer editor and handle ownership

All core editor mutations occur synchronously on the main thread inside the `Editor` struct in `ox-editor`.

### Handle model

Buffer, window, and tabpage resources are identified by positive 1-based integer handles:

- `BufHandle`: represents a text buffer.
- `WinHandle`: represents a window viewport displaying a buffer.
- `TabHandle`: represents a tabpage grouping multiple windows.

Handles are managed through strongly typed stores in `ox-editor`. Handle lookups validate existence and return scoped references. When a buffer or window is deleted, its handle is unregistered and subsequent API calls with that handle return `ApiError::InvalidHandle`.

### Text buffer and undo model

Text storage in `ox-text` is backed by Ropey, providing logarithmic-time line index lookups, insertions, and deletions. Buffers maintain:

- A piece-tree rope structure for text manipulation.
- A line index tracking byte and character offsets.
- A transactional undo tree recording edits, timestamps, and cursor positions.
- Swapfile and ShaDa serialization routines for session state persistence.

## Event reactor and multi-queue dispatch

The `ox-loop` crate implements the event reactor driving the editor process.

### Reactor loop

The event loop uses `mio` to poll non-blocking file descriptors, network streams, and signal notifications on a single thread. Timers are stored in a binary heap ordered by monotonic deadlines. The reactor calculates the next poll timeout from the earliest timer deadline.

### MultiQueue scheduling

To prevent callback re-entrancy and preserve execution order, `MultiQueue` divides work into two distinct priority tiers:

- Fast queue: handles immediate I/O events, RPC packet decoding, timer expirations, and user input processing.
- Deferred queue: holds callbacks scheduled for execution at safe synchronization points, such as `vim.schedule` closures and asynchronous Lua handlers.

Deferred callbacks run only when the editor is in an idle, non-reentrant state, matching the safety model of Neovim.

### Signal integration

Operating system signals are captured using `signal-hook` and piped into a dedicated event channel registered with the mio reactor, triggering clean loop interruption and signal dispatch.

## Pure-Rust vim.uv engine

The `ox-uv` crate provides a complete implementation of the Libuv API required by Neovim's `vim.uv` and `luv` modules without binding to C libuv or introducing an asynchronous runtime like tokio.

### Handle lifecycle

`ox-uv` manages handle lifecycles for:

- Timers: high-resolution timers registered with the `ox-loop` timer heap.
- Network streams: TCP listeners, TCP streams, and UDP sockets polled directly by mio.
- Unix pipes: local inter-process communication channels.
- Child processes and PTYs: process spawning with standard I/O redirection and pseudo-terminal allocation via `rustix` and `portable-pty`.
- Filesystem watchers: directory and file change notifications.

### Thread pool for filesystem I/O

Asynchronous filesystem functions (`fs_open`, `fs_read`, `fs_write`, `fs_stat`, `fs_readdir`) execute on a dedicated worker pool. When a worker completes an operation, it posts the result to the main reactor's deferred queue, ensuring Lua callbacks execute on the main thread.

## Lua host and plugin runtime

The `ox-lua` crate embeds the LuaJIT engine via `mlua` to execute Neovim Lua plugins and core runtime scripts.

### Bidirectional type conversion

The type converter translates between Rust `ox_types::Object` values and Lua types:

- Primitive conversions: Booleans, integers, floating-point numbers, and strings map directly to Lua types.
- Byte strings: Raw byte sequences (including invalid UTF-8) are represented by `OxStr` and passed as Lua strings.
- Containers: Sequential numeric tables become arrays, while string-keyed or mixed tables become dictionaries.
- Sentinels: Special sentinels `vim.NIL` and `vim.empty_dict()` retain their identity across round trips.
- Functions: Lua functions passed to API methods are tracked in a registry-backed `LuaRef` table, preserving identity.

### Execution safety and guards

Oxvim enforces execution contexts to prevent unsafe re-entrancy:

- Fast callbacks: Callbacks marked as fast run in restricted mode and cannot mutate editor buffers or call blocking APIs.
- Textlock and fastlock: Non-fast API methods verify locks before executing. Attempting to call state-mutating APIs from invalid contexts produces an `E5560` error.

### Standard library and Tree-sitter

`ox-lua` loads Neovim's core Lua modules from the `runtime/` tree:

- Preloaded modules: `vim._core`, `vim.inspect`, `vim.treesitter`, and `vim.uv` are available during startup.
- API registry: `vim.api` routes function calls to native `ox-api` implementations.
- Tree-sitter: Parsers, queries, and syntax tree inspections bind to the `tree-sitter` Rust crate.

## Declarative code generation

To maintain strict alignment with Neovim, Oxvim uses automated code generation for configuration options, Ex commands, and API signatures.

- Options: `codegen/upstream/options.lua` defines the option inventory, scope rules (global, buffer-local, window-local), types, and flags. Build scripts generate static lookup tables in `ox-editor`.
- Ex commands: `codegen/upstream/ex_cmds.lua` defines command names, range rules, argument flags, and handlers. Build scripts generate parser match tables in `ox-excmd`.
- Builtin functions: `codegen/upstream/eval.lua` defines Vimscript function signatures, parameter counts, and evaluation routes for `ox-eval`.
- API metadata: `crates/ox-rpc/src/api_metadata.msgpack` contains the canonical Neovim API Level 15 metadata used to generate function dispatch tables in `ox-api`.

## Differential verification framework

Oxvim verifies its behavior using an automated differential testing framework in `tests/differential/`.

### Oracle verification

A compiled Neovim 0.13.0-dev binary (`.references/neovim/build/bin/nvim`) serves as an executable ground-truth oracle. Tests compare Oxvim output directly against the oracle under identical inputs.

### Verification layers

The framework employs four verification mechanisms:

1. API schema diff (`apidiff`): Compares the MessagePack payload of `oxvim --api-info` against the oracle, verifying all 262 functions, parameter flags, error types, and metadata fields.
2. Session replay (`replay`): Executes YAML-defined MessagePack-RPC session transcripts (`core.yaml`, `options.yaml`, `eval.yaml`, `channels.yaml`, `ui_attach.yaml`) against both binaries, checking response data and notification ordering.
3. Interactive PTY smoke tests: Spawns the `oxvim` binary in a real pseudo-terminal, driving keyboard input, modal transitions, Ex commands, and verifying terminal restoration sequences on exit.
4. Upstream test suites: Harness targets `just functional` and `just oldtest` run Neovim's functional and legacy test suites against Oxvim using the `NVIM_PRG` environment variable.

### Sanctioning policy

Any behavioral divergence in session replay must be justified and recorded with a SHA-256 stream fingerprint in `tests/differential/SKIPS.md`. Unsanctioned divergence causes immediate test failure.

## Terminal user interface and client separation

The bundled terminal interface in `ox-tui` is implemented as an independent client connecting to the embedded server.

### Client-owned surfaces

The TUI client renders chrome only on surfaces externalized by the Neovim UI protocol:

- Command line (`ext_cmdline`): Draws input text, prompt symbols, and cursor position.
- Messages (`ext_messages`): Displays messages, errors, and interactive prompts.
- Completion popup (`ext_popupmenu`): Renders completion candidate lists and selection state.

### Server-rendered surfaces

Surfaces not externalized by the protocol remain rendered by the server compositor in `ox-ui`:

- Statusline, tabline, and winbar.
- Floating window borders, titles, and backgrounds.
- Sign column and line number column.
- Welcome and intro screens.

The client composites server-rendered grid cells directly without adding unrequested framing or altering cell contents.

### Theme synchronization and terminal restore

Client chrome synchronizes dynamically with the active colorscheme on `hl_attr_define` batch boundaries. On exit, `ox-tui` restores the terminal palette (OSC 104), resets the cursor shape, disables mouse tracking, and returns the terminal to cooked mode.
