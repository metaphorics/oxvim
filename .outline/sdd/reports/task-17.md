# Task 17 — runner path/filesystem builtins

## Status

BLOCKED before busted spec execution by `setenv()`, which is the first newly surfaced builtin after the requested pure path/filesystem family. The runner now passes its `fnamemodify()` call at `test/runner.lua:8` and reaches `vim.env.VIMRUNTIME = ...` at line 48.

## Commits

- `81afee7 fix(oxvim): name -l chunks with @ prefix`
- `7eb8cf6 feat(ox-eval): implement path and filesystem builtins`
- `2a6bc1a feat(ox-eval): add glob and executable builtins`
- `18b033a fix(ox-eval): match filename modifier separator semantics`

## Implemented behavior

- `fnamemodify()`: `:8`, `:p`, `:~`, `:.`, repeated `:h`, `:t`, `:r`, `:e`, `:s`, `:gs`, and `:S`, including chained modifiers, dotfiles, trailing separators, full paths, home/current-directory shortening, and regex substitution through the existing `RegexEngine` seam.
- Filesystem/path family: `filereadable()`, `isdirectory()`, `getcwd()`, `resolve()`, and `simplify()`.
- Expansion/search family: `glob()`, `globpath()`, and `executable()`, including string/list returns, sorted wildcard results, `**`, path lists, dangling-link `alllinks`, explicit executable paths, and Unix `PATH`/mode-bit lookup.

## Upstream sources

- `.references/neovim/runtime/doc/cmdline.txt:1038-1126` — filename modifier set, ordering, repetition, and examples.
- `.references/neovim/runtime/doc/vimfn.txt:2444-2464,2751-2765,4805-4905,8581-8592,10524-10542` — filesystem/path/glob contracts.
- `.references/neovim/runtime/doc/vimfn.txt:1957-1988` — executable lookup contract.
- `.references/neovim/src/nvim/eval/funcs.c:6362-6381` — `setenv()` mutates or removes the process environment.

## Verification

- `cargo nextest run -p ox-eval -p ox-editor -p oxvim`: **815 passed, 0 skipped**.
- Focused `ox-eval` run after the final modifier correction: **353 passed, 0 skipped**.
- `cargo build -p oxvim`: passed.
- `target/debug/oxvim -l .references/neovim/test/runner.lua`: exits 1 at `test/runner.lua:48` with `E117: Function is not implemented: setenv`; no busted pass/fail counts print.

## Blocker

`setenv()` is not a pure builtin. Upstream mutates the process environment and treats `v:null` as deletion (`funcs.c:6362-6381`). This workspace uses Rust edition 2024 and every owned crate forbids unsafe code; `std::env::set_var` and `remove_var` are unsafe. The repository already records the same limitation in `crates/ox-uv/src/misc.rs:194-216`: `os_setenv()`/`os_unsetenv()` return typed `Unsupported` because neither std nor rustix exposes a safe setter. Faking a Lua-only value would not satisfy upstream semantics or propagate variables to harness child processes. Runner progress therefore requires a project-level decision outside this pure-builtin task: permit a narrowly audited unsafe environment mutation boundary or adopt a safe dependency that owns that boundary.

## Concerns

- Glob expansion intentionally implements the requested std-only core. It does not apply editor option state (`'suffixes'`, `'wildignore'`, `'wildignorecase'`) because the typval-only builtin host has no option seam.
- On non-Unix targets, `executable()` currently follows the documented existence/non-directory rule but does not implement Windows `PATHEXT` expansion.
