# ADR-001: Own Working Directory State in the Editor Model

- Status: Accepted
- Date: 2026-08-31
- Related issue: https://github.com/metaphorics/oxvim/issues/23
- Related solution: [Working Directory State Owner Solution](../solutions/architecture/current-directory-state-owner.md)

## Context

Vim and Neovim support scoped working directories. Commands like `:cd` change the global working directory. Commands like `:lcd` change the window-local working directory. The `:cd -` and `:lcd -` commands toggle to the previous working directory for that scope.

In Oxvim, the editor core (`Editor`) and the window layouts (`Layout`, `WindowState`) did not own working directory state. Instead, commands mutated the operating system process working directory directly without tracking scope or history.

This design caused several defects:
1. Window switches did not restore window-local directories set by `:lcd`.
2. Tab switches and buffer switches failed to synchronize the active directory.
3. The `:cd -` and `:lcd -` commands could not resolve previous directory paths.
4. Model lifecycle transitions (such as `Editor::close_tabpage`) bypassed command-level wrappers and left stale process directories.

Oxvim requires an explicit architecture for working directory state ownership, transition triggers, and error behavior.

## Decision

We place working directory state ownership in the editor model and treat the operating system working directory as a cached value.

### 1. State ownership

- `Editor` owns session-global working directory state:
  - `global_directory: Option<PathBuf>` stores the session-global fallback directory. It initializes lazily on first tabpage creation.
  - `previous_directory: Option<PathBuf>` stores the previous directory left by the last global `:cd` or `chdir(path, "global")`.
- `WindowState` owns window-local working directory state:
  - `local_directory: Option<PathBuf>` stores the window-local directory set by `:lcd` or `chdir(path, "window")`.
  - `previous_directory: Option<PathBuf>` stores the previous directory left by the last `:lcd` in this window.
- `ExRuntime` and `ExExecutor` own no working directory state. They parse command arguments and call `Editor` methods.

### 2. Operating system directory as a best-effort cache

The operating system process working directory is updated via `std::env::set_current_dir` as a best-effort cache of the current window effective directory:
- Effective directory resolution:
  - If the active window has a `local_directory`, use it.
  - Otherwise, if `global_directory` exists, use it.
  - Otherwise, leave the process working directory unchanged.
- Transition synchronization:
  - The editor invokes `apply_effective_directory()` after every state change that affects the active window:
    - Window focus changes (`set_current_window`)
    - Tabpage focus changes (`set_current_tabpage`)
    - Window close operations (`close_window`)
    - Tabpage close operations (`close_tabpage`)
    - Buffer replacements in the active window (`set_window_buffer`)
    - Window splits (`split_window_horizontal`, `split_window_vertical`)
    - Tabpage insertions (`insert_tabpage`)

### 3. Explicit changes versus automatic transitions

- Explicit directory changes (`:cd`, `:lcd`, `chdir()`):
  - Resolve the owner and target scope first; a missing current window during a window-scoped change returns `E16` before any operating system or model mutation.
  - Then call `std::env::set_current_dir`; operating system failure returns `E344` and leaves model directory state unchanged.
  - Update `previous_directory` and `local_directory` / `global_directory` only after successful operating system transition.
  - Clear `local_directory` on the active window when a global `:cd` succeeds, matching upstream Neovim.
- Automatic transitions (`apply_effective_directory`):
  - Match upstream Neovim `update_cwd` (`window.c:5365`).
  - The model transition has already committed. Therefore, an operating system `chdir` failure never rolls back the model state.
  - Failures are silent and best-effort. The editor does not queue or raise `E344`. The process stays at its prior working directory.

### 4. Window inheritance and scope isolation

- New tabpages (`insert_tabpage`) and window splits inherit `local_directory` and `previous_directory` from the source window, matching upstream `win_alloc`.
- Tabpage-local (`t_localdir`) and buffer-local (`b_localdir`) scopes are outside this decision and distinct from window-local state; they are not aliased or collapsed into window state.

### 5. Verification contract

Regression tests must verify state transitions directly on `Editor` APIs, not only through Ex command strings. Ex command wrappers can mask missing model lifecycle hooks (such as `:tabclose` calling `set_current_tabpage` after `close_tabpage`).

## Alternatives Considered

### 1. Executor-owned state
Store working directory maps in `ExRuntime` or `ExExecutor`.

Rejection reason: The Ex command executor is not the lifecycle owner of editor windows and buffers. Direct API calls, RPC requests, and internal window layout operations bypass the executor. Placing state in the executor leads to state drift.

### 2. Buffer-owned window state
Store window-local directory state on `Buffer` or `BufferState`.

Rejection reason: In Vim architecture, `:lcd` belongs to windows, not buffers. A buffer can appear simultaneously in multiple windows with different local directories. Storing directory state on buffers breaks window isolation.

### 3. Queued automatic E344 errors
Record operating system `chdir` failures during window transitions and surface `E344` on the next user command.

Rejection reason: Upstream Neovim `update_cwd` silently ignores operating system transition failures once a window or tab transition commits. Queuing errors surfaces confusing, delayed failures during normal window navigation.

## Consequences

### Positive
- `Editor` and `WindowState` provide a single source of truth for global and window-local working directory state.
- Window, tab, and buffer transitions synchronize the process working directory automatically.
- Explicit directory commands maintain atomic semantics and produce accurate Vim error codes.
- Direct model API calls and Ex command invocations share identical working directory behavior.

### Negative and trade-offs
- The operating system working directory is a best-effort cache. If filesystem permissions prevent `set_current_dir`, the process directory can diverge silently until the next explicit change.
- Tabpage-local and buffer-local directory scopes remain outside this decision and distinct from window-local state.
