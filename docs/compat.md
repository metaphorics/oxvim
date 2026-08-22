# Compatibility statement

This document details the compatibility status of Oxvim against upstream Neovim, summarizing verified subsystems, active test harnesses, and current architectural limitations.

## Target specification

Oxvim targets functional compatibility with Neovim 0.13.0-dev (API level 15).

The primary objective is to serve as a drop-in replacement for the Neovim core that executes existing Lua plugins, supports external MessagePack-RPC user interfaces, and maintains exact behavioral semantics across text manipulation, options, expressions, and event loops.

## Verified subsystems

Subsystem compatibility is verified through automated unit tests, differential schema comparisons, RPC transcript replays, and pseudo-terminal execution checks.

### Workspace test suites

Over 1,400 unit, property, and integration tests pass across all workspace crates:

- `ox-types`: Object enums, handle types, typval conversions, and error models.
- `ox-rpc`: MessagePack encoding, decoding, request dispatch, and notification streams.
- `ox-loop`: Event reactor polling, timer heaps, and MultiQueue priority scheduling.
- `ox-uv`: File descriptor management, asynchronous filesystem thread pool, networking, and signal handling.
- `ox-text`: Piece-tree rope operations, line indexing, transactional undo trees, and ShaDa parsing.
- `ox-regex`: Backtracking and NFA regex pattern matching.
- `ox-eval`: Vimscript expression parsing, operator precedence, and variable scopes.
- `ox-excmd`: Ex command syntax parsing and range resolution.
- `ox-editor`: Single-writer editor state, window layout splits, and registers.
- `ox-api`: Strongly typed `nvim_*` API functions and parameter conversion.
- `ox-lua`: LuaJIT state initialization, type conversion, and standard library modules.
- `ox-ui`: Grid cell compositing, line grid protocol generation, and highlight attributes.
- `ox-tui`: Terminal event parsing, screen rendering, and mode switches.
- `oxvim`: Command-line interface, server loop orchestration, and client startup.

### API metadata parity

The `just apidiff` check validates the `--api-info` output against the compiled Neovim oracle binary. The schema matches exactly:

- All 262 public API functions are declared with identical parameter types, return types, deprecation markers, and method associations.
- Parameter optionality flags and execution mode annotations match upstream declarations.
- Error type enumerations and UI event definitions align with the oracle schema.

### MessagePack-RPC differential replay

The `just replay` harness executes recorded YAML RPC sessions against both Oxvim and the Neovim oracle, verifying stream data and notification ordering:

- Core operations (`core.yaml`): Buffer creation, line updates, Lua feature detection, normal mode commands (`normal! ggdd`), and final buffer contents match upstream responses.
- Options (`options.yaml`): Boolean, integer, string, flag, comma-separated list, and colon-delimited map option assignments match upstream behavior. Structured dictionary and array return values from `nvim_set_option_value` match upstream return shapes and type validation checks.
- Vimscript evaluation (`eval.yaml`): Arithmetic calculations, string functions, list indexing, and dictionary lookups match upstream evaluation streams.
- Channels (`channels.yaml`): Terminal channel allocation, socket streams, `nvim_chan_send` routing, and unknown-method error event generation match upstream behavior.
- UI attachment (`ui_attach.yaml`): Initial redraw events are emitted prior to the attach response. Negotiated extension options, default colors, initial highlight tables, and mode info events are verified with a narrow sanctioned difference for renderer-specific metadata.

### Interactive terminal execution

The `just differential` suite runs automated PTY tests against the release binary in a simulated terminal environment:

- Spawns Oxvim in raw terminal mode.
- Sends keystrokes to enter text in Insert mode.
- Executes Ex commands (`:echo 1+1<CR>`) through the command line and verifies rendered output.
- Executes quit commands (`:q!<CR>`) and verifies clean process exit with status code 0.
- Asserts terminal cleanup sequences, including cursor shape restoration and palette reset (OSC 104).

## Work in progress

Two upstream test suites are being incrementally enabled:

- Upstream functional test suite (`just functional`): Runs Neovim's Busted-based functional test suite against the Oxvim binary using the `NVIM_PRG` harness override.
- Upstream legacy test suite (`just oldtest`): Runs the legacy Vimscript test harness against the Oxvim binary using `NVIM_PRG`.

## Known gaps and current limitations

The following limitations are documented and tracked for upcoming development phases:

### Option mutation operations

`nvim_set_option_value` supports direct value assignment across all option types. However, composite mutation operations (`append`, `prepend`, and `remove`, corresponding to `:set+=` and `:set-=`) currently return a typed `Unsupported` error. Additionally, unsetting local option values via `nil` is not yet supported in the editor options store.

### Expression and callback mappings

Key mappings, `<Nop>`, and parsed Ex-command mappings execute through the interactive input loop. Mappings that evaluate Vimscript expressions (`Expr`) or invoke Lua callbacks (`Callback`) return a typed API error during interactive input dispatch.

### Process-backed job control

In-process terminal channels (`nvim_open_term`, `nvim_chan_send`) are connected and routed. Full asynchronous process execution via `jobstart()` with stdout/stderr callback routing and terminal VT escape sequence parsing into buffer cells is under active development.

### Advanced vim.uv features

The `ox-uv` engine implements the core handle types and asynchronous filesystem operations. Advanced features, including custom polling priorities, non-standard file descriptor redirection, and platform system metrics, return typed `Unsupported` errors rather than failing silently.

### Platform support

Oxvim is tested and verified on Linux and POSIX-compliant operating systems. Windows platform compatibility is currently untested.
