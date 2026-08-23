# Task 76: plugin-ecosystem probe

Status: **done.** Probe only — no file under `crates/` or `runtime/` was changed. The findings live
in [`.outline/sdd/plugin-probe.md`](../plugin-probe.md); this is the short account.

Binary under test: `target/release/oxvim` built from `5a2105f` (`OXVIM v0.13.0`, API level 15,
Release). Oracle: `.references/neovim/build/bin/nvim`, `v0.13.0-dev-1390`, API level 15. `ox-tui`
changed on `main` after this binary was built; every probe here is `--headless`, so nothing measured
touches the TUI.

Network was **available**. `lazy.nvim` `306a055`, `plenary.nvim` `74b06c6`, `tokyonight.nvim`
`cdc07ac`, `telescope.nvim` `40aedd8` and `nvim-treesitter` `8b98b44` were cloned fresh from GitHub
during the probe. Every run used its own throwaway directory under `/tmp/t76probe/` with its own
`HOME` and `XDG_*`; no probe saw the real home directory or a real user config, and nothing was
written inside `.references`.

## Ladder

| rung | result |
| --- | --- |
| 1a Lua host, `package.path` require | **reached** — LuaJIT 2.1, `bit`, `ffi`, oracle-identical |
| 1b `require` a module on `'runtimepath'` | **failed** — `module 'cfgmod' not found`; `nvim_list_runtime_paths()` returns `{}` |
| 2 lazy.nvim bootstrapping itself | **failed at step 1 of 6** — `E117: stdpath`, then `system`, then `vim.opt.rtp:prepend`, then `require` |
| 3 pure-Lua plugin | **reached with the rtp loader shimmed** — plenary loads fully and runs 2/2 specs with oracle-identical output; tokyonight loads, `load()` dies on `stdpath` |
| 4 telescope.nvim | **modules load with the shim; `setup()` fails** on `E117: getenv` |
| 5 treesitter | **string parsing, queries and node text are oracle-identical; buffer parsing unavailable** |

## Ranked blockers

1. **No rtp-based runtime-file search.** `nvim__get_runtime` is bound to a single root
   (`crates/ox-lua/src/embedded.rs:31-49`) and `RuntimeState::runtime_paths` is never populated at
   runtime (`crates/ox-api/src/runtime.rs:126-129,181-185`; `set_runtime_files` has no non-test
   caller). Kills all `require`, `:runtime`, autoload, `nvim_get_runtime_file`, `vim.loader`, and
   treesitter parser discovery by name.
2. **The Lua `vim.fn` bridge falls back to `Builtins::without_regex()`**
   (`crates/oxvim/src/server.rs:1165-1188`). 24 of the 45 builtins probed work in Vimscript and
   answer `E117` from Lua; every regex builtin answers `E54: regular-expression engine is not
   installed`.
3. **`vim.cmd` and command/mapping registration are unimplemented.** 42 of the 165 `vim.api` names
   oxvim shares with the oracle answer `API function is not implemented`, including `nvim_exec2`,
   `nvim_cmd`, `nvim_exec_lua`, `nvim_create_user_command`, `nvim_set_keymap` and `nvim_get_keymap`.
4. **User config is never discovered.** Only an explicit `-u` is sourced
   (`crates/oxvim/src/server.rs:200`); `plugin/` is never sourced at all.
5. **`vim.fn.jobstart` from Lua panics** — `unreachable!()` at
   `crates/ox-editor/src/builtins/process.rs:112`, reached because `server.rs:1168` routes
   `"jobstart"` into `ExExecutor::call_builtin` → `call_job_builtin`, which has no `jobstart` arm.
6. **`vim.opt.X:append/prepend/remove`** rejected by `nvim_set_option_value`
   (`crates/ox-api/src/global.rs:963-975`), which the vendored upstream `vim/_core/options.lua`
   depends on.
7. **20 builtins missing everywhere**, Lua and Vimscript alike — `stdpath`, `getenv`, `environ`,
   `mode`, `shellescape`, `fnameescape`, `strdisplaywidth`, `localtime`, `reltime`, `reltimefloat`,
   `reltimestr`, `termopen`, `confirm`, `visualmode`, `screenrow`, `screencol`, `searchcount`,
   `matchadd`, `sign_define`, `complete`.
8. **Treesitter buffer parsing** — `expected either string or buffer handle; buffer parsing is
   unavailable`.

Smaller seams: `--clean` plus `-u <file>` ignores the file; `expand('%:p')` returns the literal
`'%:p'`; an unknown function aborts the script with exit 1 instead of raising a catchable `E117`;
`vim.diagnostic` fails to load with `Wrong number of arguments: expecting 2 but got 1`.

## Method note

Blocker 2 is a three-branch dispatch, so it was proved with one case per branch, each arranged to
give a different answer if the branch it targets were wrong: the job branch (`vim.fn.jobstart`)
panics, the buffer branch (`vim.fn.getline`/`setline`) succeeds, the fallback branch answers `E117`
for a file-IO builtin and `E54` for a regex builtin. A single probe against the fallback alone would
have reported "40 missing functions" and hidden both the panic and the fact that the functions exist.

Where a rung was blocked by a defect a lower rung had already reported, it was re-run with that one
primitive stubbed from the Lua side, so the tail behind the blocker could be measured. Those results
are labelled as shimmed in the full report and are evidence about what a fix buys, not a claim that
the rung passes today.

## Verdict

The premise does not hold today: nothing in the ecosystem loads, and it fails at the first move.
But the wall is thin. Behind the rtp loader, plenary.nvim runs its own test harness with
byte-identical output to the oracle, every module of lazy/telescope/tokyonight/treesitter requires
cleanly, and buffers, floats, extmarks, autocmds, `vim.uv`, `vim.fs`, `vim.iter` and a real compiled
tree-sitter parser all behave. What is missing is concentrated and structural — one rtp search, one
`vim.fn` bridge, ~42 API functions dominated by `vim.cmd` and mapping/command registration, one
config-discovery step, one `unreachable!()`, and ~20 unwritten builtins — not a long tail of
behavioural divergence.
