# Oxvim: Neovim rewritten in Rust

Oxvim is a Rust implementation of the Neovim core that preserves the existing Lua plugin ecosystem. Built as a high-performance Neovim rewrite, Oxvim provides a Neovim-compatible editor architecture written in Rust. It executes native Neovim Lua plugins, speaks MessagePack-RPC, and integrates a pure-Rust event reactor without C dependencies.

## Status

Oxvim is in active pre-release development. All core editor subsystems are implemented and verified through unit suites, interactive terminal checks, and differential replay harnesses against an upstream Neovim oracle binary.

- Target specification: Neovim 0.13.0-dev, API level 15.
- API schema parity: `just apidiff` passes with zero unsanctioned schema differences against the Neovim oracle.
- MessagePack-RPC differential replay: `just replay` verifies exact stream matching across core buffer manipulation, options handling, Vimscript evaluation, and channel I/O.
- Test coverage: over 1,400 unit, property, and integration tests pass across the workspace.
- Upstream functional and legacy test suite compatibility (`just functional`, `just oldtest`) is in progress.

For a detailed breakdown of verified surfaces and current limitations, see the [compatibility statement](docs/compat.md).

## Architecture summary

The workspace is organized into focused crates, separating core editor data structures, event loops, Lua runtimes, and user interface layers.

| Crate | Purpose |
| --- | --- |
| `ox-types` | Core `Object` enum, handle types (`BufHandle`, `WinHandle`, `TabHandle`), `ApiError`, and typval conversion model |
| `ox-rpc` | MessagePack-RPC codec, channel state machine, and canonical API metadata decoding |
| `ox-loop` | Event reactor built on mio, monotonic timer heap, and MultiQueue fast and deferred scheduling queues |
| `ox-uv` | Pure-Rust implementation of the `vim.uv` API supporting handles, process spawning, PTYs, networking, and filesystem thread pools |
| `ox-text` | Text buffer backed by Ropey, line indexing, transactional undo tree, swapfile logic, and ShaDa persistence |
| `ox-regex` | Vim regex engine supporting backtracking and NFA execution strategies |
| `ox-eval` | Vimscript expression lexer, parser, evaluator, and scope resolution |
| `ox-excmd` | Ex command parser and generated command metadata |
| `ox-editor` | Single-writer editor state, window layout tree, options store, registers, and marks |
| `ox-api` | Strongly typed `nvim_*` API implementations generated and annotated via `ox-api-macros` |
| `ox-lua` | Embedded LuaJIT host via mlua, bidirectional Object and Typval converters, standard library loaders, and Tree-sitter bindings |
| `ox-ui` | Server-side UI event emission, multi-grid layout engine, and redraw compositor |
| `ox-tui` | Redesigned bundled terminal client built on crossterm and mio |
| `oxvim` | Binary entry point, command-line parsing, embedded server loop, and client process supervisor |

For in-depth architectural contracts and subsystem design, see the [architecture documentation](docs/architecture.md).

## Pure-Rust event loop and vim.uv

Unlike C implementations that rely on libuv or async frameworks that require multi-threaded runtimes like tokio, Oxvim implements its event reactor and Lua I/O layer in pure Rust.

The `ox-loop` crate drives a single-threaded reactor using `mio` and POSIX primitives. Scheduling is managed by `MultiQueue`, which prioritizes immediate fast-path events (such as I/O notifications and timer expiries) while queuing deferred callbacks (such as `vim.schedule` invocations and Lua hooks) to run at safe execution points.

The `ox-uv` crate exposes the `vim.uv` API surface to Lua plugins. File operations run on a dedicated worker pool, while timers, child processes, PTY allocations, Unix domain sockets, and TCP streams register with the main mio poll instance. This eliminates foreign C runtimes while matching Neovim's single-threaded concurrency guarantees.

## Building and installation

### Prerequisites

Building Oxvim requires:

- Rust 1.98 or later with Cargo
- The `just` command runner
- Standard C toolchain for vendored LuaJIT compilation

### Compilation

Clone the repository and build the release binary:

```sh
cargo build --workspace --release
```

The compiled binary is placed at `target/release/oxvim`.

### Runtime files and code generation

Oxvim uses two asset trees:

- `runtime/`: Vendored runtime files containing Neovim Lua core modules, syntax definitions, filetype plugins, and documentation.
- `codegen/upstream/`: Declarative specification tables (`eval.lua`, `ex_cmds.lua`, `options.lua`) used by build scripts to generate compile-time Rust tables for options, Ex commands, and builtin functions.

## Development workflow

Oxvim uses `just` to manage development, testing, and differential verification tasks.

| Target | Description |
| --- | --- |
| `just build` | Compile the entire workspace in release mode |
| `just test` | Run the complete workspace unit and integration test suite |
| `just apidiff` | Validate `oxvim --api-info` schema against the Neovim oracle binary |
| `just replay` | Replay semantic MessagePack-RPC test sessions against both binaries |
| `just differential` | Run release binary smoke checks and interactive PTY differential tests |
| `just functional` | Execute upstream Neovim functional tests against Oxvim via `NVIM_PRG` |
| `just oldtest` | Execute upstream Neovim legacy Vimscript tests against Oxvim via `NVIM_PRG` |

## Compatibility

Oxvim targets complete functional compatibility with Neovim 0.13.0-dev (API level 15).

- Core API metadata generated by `oxvim --api-info` matches the upstream Neovim schema.
- Interactive PTY tests verify terminal initialization, modal editing, Ex command execution, and terminal teardown.
- Upstream functional and oldtest test suites are being enabled to verify plugin ecosystem parity.

See [docs/compat.md](docs/compat.md) for full compatibility status and known limitations.

## License and attribution

Oxvim's original Rust codebase is licensed under the Apache License, Version 2.0.

The `runtime/` tree and reference materials include code derived from Neovim and Vim, which are distributed under their respective original licenses.

### Upstream attribution

Copyright Neovim contributors. All rights reserved.

Neovim is licensed under the terms of the Apache 2.0 license, except for parts of Neovim that were contributed under the Vim license.

Third-party libraries used or referenced by upstream Neovim include:

- libvterm: MIT license
- utf8proc: MIT license
- lua-lpeg: MIT license
- tree-sitter: MIT license
- Vim runtime and base code: Vim license
