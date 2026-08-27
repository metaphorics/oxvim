# Plugin-ecosystem probe: how far real Neovim plugins get on oxvim

Status: **probe only, no source changed.** Every number below was observed in this session.

## Setup

| | |
| --- | --- |
| Binary under test | `target/release/oxvim`, built from `5a2105f` (`OXVIM v0.13.0`, API level 15, Release) |
| Oracle | `.references/neovim/build/bin/nvim`, `v0.13.0-dev-1390`, API level 15 |
| Network | **available** — every plugin below was cloned fresh from GitHub during the probe |
| Isolation | one directory per probe under `/tmp/t76probe/<name>`, with its own `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`. No probe saw the real home directory or a real user config. Nothing was written inside `.references`. |

Plugins probed, at the commits that were current during the probe:

| plugin | sha | why it is on the ladder |
| --- | --- | --- |
| `folke/lazy.nvim` | `306a055` | the current standard plugin manager; bootstraps itself |
| `nvim-lua/plenary.nvim` | `74b06c6` | pure Lua, plus the `busted` test harness half the ecosystem runs on |
| `folke/tokyonight.nvim` | `cdc07ac` | pure-Lua colorscheme, very few API calls |
| `nvim-telescope/telescope.nvim` | `40aedd8` | API-heavy: plenary, windows, floats, keymaps |
| `nvim-treesitter/nvim-treesitter` | `8b98b44` | needs parsers and a compiled runtime |

`ox-tui` changed on `main` after this binary was built. Nothing probed here touches the TUI —
every run is `--headless` — so the measurements stand for `5a2105f`.

Where a rung was blocked by something a lower rung had already reported, the probe was re-run with
that primitive stubbed out from Lua, so the tail behind the blocker could be measured too. Those runs
are marked **(shimmed)** and the shim is named. A shimmed result is evidence about what a fix would
buy, not a claim that the rung passes today.

---

## The ladder

### Rung 1a — Lua host and `package.path`: **reached**

`require` of a pure-Lua module through `package.path`, with no editor calls.

```
PURE_REQUIRE=true fib10=55
LUAVER=Lua 5.1 JIT=LuaJIT 2.1.1780076327
BIT=true FFI=true
```

Identical to the oracle apart from the LuaJIT patch number. The Lua host itself is not the problem.

### Rung 1b — `require` of a module on `'runtimepath'`: **failed**

This is the mechanism every plugin is loaded by, and it does not work. A module placed in
`$XDG_CONFIG_HOME/nvim/lua/cfgmod/init.lua` — the most ordinary location there is — is not found:

```
module 'cfgmod' not found:
	no field package.preload['cfgmod']
	no file '.../target/release/../../runtime/lua/cfgmod.lua'
	no file '.../target/release/../../runtime/lua/cfgmod/init.lua'
	no file './cfgmod.lua'
	no file '/usr/local/share/luajit-2.1/cfgmod.lua'
	...
```

The oracle finds it (`CFG_REQUIRE=true cfg-ok`). `'runtimepath'` itself is correct — oxvim's
non-`--clean` rtp matches the oracle's entry for entry. What is missing is the search over it:

| probe | oxvim | oracle |
| --- | --- | --- |
| `vim.o.runtimepath` | 13 entries, matches oracle | 14 entries (extra `build/lib/nvim`) |
| `nvim_list_runtime_paths()` | `{}` | 14 paths |
| `nvim_get_runtime_file('lua/cfgmod/init.lua', true)` | `{}` | the file |
| `nvim__get_runtime({'lua/cfgmod/init.lua'}, false, {is_lua=true})` | `{}` | the file |
| `#package.loaders` | 5, `loaders[2]` is `vim._load_package` | same |

The loader is installed and the upstream `vim/_init_packages.lua` in `runtime/` is unmodified; it
calls `vim.api.nvim__get_runtime`, which returns nothing. Two independent causes, both in source:

- `crates/ox-lua/src/embedded.rs:31-49` binds `nvim__get_runtime` to a single `RuntimeRoot`
  ("`RuntimeRoot` already represents the single Lua-aware runtime root"), i.e. `$VIMRUNTIME` only.
  `'runtimepath'` is never consulted. This is what breaks `require`.
- `crates/ox-api/src/runtime.rs:126-129,181-185` populates `RuntimeState::runtime_paths` from the
  `OXVIM_REF_ROOT` env var or `set_runtime_files()`. `set_runtime_files` has no caller outside
  tests, so at runtime the vector is empty and `nvim_list_runtime_paths` /
  `nvim_get_runtime_file` (`crates/ox-api/src/channel.rs:93-112`) return `{}`.

The same missing search takes down everything else that walks the rtp:

| mechanism | oxvim | oracle |
| --- | --- | --- |
| `$XDG_CONFIG_HOME/nvim/init.lua` auto-sourced | no | yes |
| `$XDG_CONFIG_HOME/nvim/init.vim` auto-sourced | no | yes |
| `-u <file>.lua` sourced | yes | yes |
| `-u <file>.vim` sourced | yes | yes |
| `plugin/*.vim` sourced at startup | no | yes |
| `plugin/*.lua` sourced at startup | no | yes |
| `:runtime plugin/hello.vim` | fails | succeeds |
| `autoload/myal.vim` → `myal#f()` | `E117` | `'autoload-ok'` |

Config discovery is a deliberate gap, not an accident — `crates/oxvim/src/server.rs:200` sources a
config only when `-u` named one, with the comment *"nothing rather than guessing platform paths"*.
`cli.loadplugins` is parsed and tested (`crates/oxvim/src/cli.rs:135-170,320-323`) but no code
sources `plugin/` anywhere.

Aside, from the same site: oxvim gates config sourcing on `if !cli.clean`, so `--clean -u file`
ignores the file entirely. The oracle honours the later `-u`. This cost a probe before it was
noticed and is worth fixing for anyone writing probes against oxvim.

### Rung 2 — lazy.nvim bootstrapping itself: **failed at step 1 of 6**

The verbatim documented bootstrap, instrumented step by step. This is the unshimmed run:

```
FAIL 1 stdpath(data)          :: E117: Function is not implemented: stdpath
     fallback lazypath=.../data/nvim/lazy/lazy.nvim
     fs_stat=false
OK   2 uv/loop fs_stat
FAIL 3 vim.fn.system git clone :: E117: Function is not implemented: system
FAIL 4 vim.opt.rtp:prepend     :: Unsupported nvim_set_option_value operation: prepend
OK   4b vim.o.rtp string prepend (fallback)
FAIL 5 require("lazy")         :: module 'lazy' not found: ...
FAIL 6 lazy.setup              :: module 'lazy' not found: ...
```

So lazy.nvim gets **nowhere**: it cannot compute its install path, cannot clone itself, cannot put
itself on the rtp, and cannot be required. Four distinct defects in the first four lines of the most
widely used config file in the ecosystem.

Step 4 is worth naming separately. `vim/_core/options.lua:298,332,340` — vendored upstream, unmodified
— implements `vim.opt.X:append/prepend/remove` by passing `{operation = 'append'|'prepend'|'remove'}`
to `nvim_set_option_value`. `crates/ox-api/src/global.rs:963-975` accepts `'set'` and explicitly
rejects the other three:

```rust
b"append" | b"prepend" | b"remove" => Err(ApiError::validation(format!(
    "Unsupported nvim_set_option_value operation: {}", ...
```

The oracle accepts all three. So the whole `vim.opt` compound-option API — not just rtp — is dead
from Lua.

**(shimmed)** With `vim.fn.stdpath` stubbed and lazy's own `lua/` put on `package.path`, lazy.nvim's
modules all load and it gets three steps further before stopping:

```
OK   require("lazy") / lazy.core.config / lazy.core.util / lazy.core.loader
FAIL lazy.setup :: E117: Function is not implemented: mkdir
       vim/loader.lua:440: in field 'enable'
       lazy.nvim/lua/lazy/init.lua:70: in field 'setup'
```

`mkdir` is not missing from oxvim — it works from Vimscript. It is unreachable from Lua. See rung 4.

### Rung 3 — pure-Lua plugins: **reached, once `require` is shimmed**

Unshimmed, both plugins fail at rung 1b: nothing can be required. **(shimmed)** with `package.path`
pointed at each plugin's `lua/`, so that only the rtp-search defect is neutralised:

| step | oxvim | oracle |
| --- | --- | --- |
| `require('tokyonight')` | OK | OK |
| `tokyonight.load()` | `E117: stdpath` at `tokyonight/util.lua:143` | OK |
| `require('plenary')`, `plenary.path`, `plenary.job`, `plenary.busted`, `plenary.async` | all OK | all OK |
| `plenary.path:new(dir):exists()` | OK | OK |
| plenary `busted` running a real spec file | **2/2 passed** | 2/2 passed |

plenary's `busted` harness is the strongest positive result in this probe. Given a spec file with
one pure-Lua assertion and one that round-trips `nvim_buf_set_lines`/`nvim_buf_get_lines`, oxvim
produced output byte-identical to the oracle's:

```
========================================
Testing: 	/tmp/t76probe/r5_ts_deep/spec_test.lua
Success	||	probe adds
Success	||	probe uses vim api
Success: 	2
Failed : 	0
Errors : 	0
========================================
```

The harness only fails on its final `qa!`, which goes through the unimplemented `vim.cmd`.

### Rung 4 — telescope.nvim: **modules load, `setup()` fails**

**(shimmed)** the same way.

| step | oxvim | oracle |
| --- | --- | --- |
| `require('telescope')` | OK | OK |
| `telescope.setup({})` | `E117: getenv` at `plenary/log.lua:12` | OK |
| `require('telescope.builtin')` | OK | OK |
| `telescope.builtin.find_files()` | `loop or previous error loading module 'telescope.config'` (cascade from `setup`) | OK |

The primitives telescope leans on were probed directly, and the picture is not uniformly bad:

| primitive | oxvim |
| --- | --- |
| `nvim_create_buf`, `nvim_buf_set_lines`, `nvim_buf_get_lines` | works |
| `nvim_open_win` with a float config | works |
| `nvim_buf_set_extmark`, `nvim_create_namespace` | works |
| `nvim_create_autocmd`, `nvim_create_augroup` | works |
| `vim.schedule`, `vim.defer_fn`, `vim.notify` | works |
| `vim.uv.fs_stat`, `vim.fs.dirname`, `vim.fs.find` | works |
| `vim.iter`, `vim.split`, `vim.tbl_deep_extend` | works |
| **`vim.cmd(...)`** | `API function is not implemented` |
| **`vim.keymap.set`, `nvim_set_keymap`** | `API function is not implemented` |
| **`nvim_create_user_command`** | `API function is not implemented` |
| `nvim_command` | `Not implemented: nvim_command executor` |
| `vim.lsp` | `E117: stdpath` on module load |

Buffers, windows, floats, extmarks and autocmds — the hard parts — are there. Commands and mappings,
the two things every plugin registers, are not.

### Rung 5 — treesitter: **parsers load, buffer parsing does not**

| step | oxvim | oracle |
| --- | --- | --- |
| `require('nvim-treesitter')` (shimmed require) | OK | OK |
| `language.add('c')` by rtp lookup | returns without error, registers nothing | registers |
| `language.inspect('c')` after that | `no such language: c` | ok |
| `language.add('c', {path=.../parser/c.so})` | **`true`** | `true` |
| `language.inspect('c')` after that | **`abi=15`** | `abi=15` |
| `get_string_parser(src,'c'):parse()` → root | **`translation_unit/1`** | `translation_unit/1` |
| `query.parse` + `iter_captures` on a string tree | **`captures=1`** | `captures=1` |
| `get_node_text` | **exact** | exact |
| `get_parser(0,'c'):parse()` (buffer) | `expected either string or buffer handle; buffer parsing is unavailable` | `translation_unit/1` |
| `vim.treesitter.start(0,'c')` | `Parser could not be created for buffer 1` | ok |

This rung is better than expected. oxvim loads a real compiled `tree-sitter` `.so`, agrees with the
oracle on ABI 15, parses source, runs queries and extracts node text — all identical. Two things are
missing: parser discovery by name (the rung-1b rtp defect again) and buffer-backed parsing.

---

## Failures ranked by how much they block

Ranking is by reach, not by size. Two orderings matter and they are not the same.

### Execution order — what a real user's first startup hits, in sequence

1. **Config discovery.** `init.lua` / `init.vim` under `stdpath('config')` is never read. Nothing
   the user wrote runs at all. `crates/oxvim/src/server.rs:200`.
2. **`vim.fn.stdpath`** — line 1 of every lazy.nvim bootstrap. Not implemented anywhere, Lua or
   Vimscript.
3. **`vim.opt.rtp:prepend`** — line 4. `crates/ox-api/src/global.rs:963-975`.
4. **`require` off the rtp** — line 5. `crates/ox-lua/src/embedded.rs:31-49`.

### Leverage order — what one fix buys

1. **rtp-based runtime-file search** (`nvim__get_runtime` bound to a single root;
   `RuntimeState::runtime_paths` never populated). Reach: all `require` of all plugin and user Lua,
   `:runtime`, autoload, `nvim_list_runtime_paths`, `nvim_get_runtime_file`, treesitter parser
   discovery by name, and `vim.loader`. Nothing in the ecosystem loads without it. Measured payoff:
   simulating this one fix client-side (`package.path`) took plenary.nvim from *nothing loads* to
   *the full library plus its test harness running 2/2 tests with oracle-identical output*, and made
   every module of lazy.nvim, telescope.nvim, tokyonight.nvim and nvim-treesitter require cleanly.
2. **The Lua `vim.fn` bridge routing to the stateless builtin table.** One wiring defect, 24
   confirmed symptoms out of the 45 builtins probed. `crates/oxvim/src/server.rs:1165-1188` dispatches
   in three branches, and the fallback is `Builtins::without_regex()` — no editor, no file IO, no
   regex engine. The functions are implemented; Lua just cannot see them. Proven with one case per
   branch, each producing a different diagnosis:

   | branch | probe | result |
   | --- | --- | --- |
   | hardcoded job list → `ex.call_builtin` | `vim.fn.jobstart({'echo','hi'})` | **panic**, see below |
   | `is_buffer_builtin` → `call_buffer_builtin` | `vim.fn.getline(1)`, `vim.fn.setline(1,'x')` | ok |
   | fallback → `Builtins::without_regex()` | `vim.fn.writefile`, `vim.fn.readfile` | `E117` |
   | fallback → `Builtins::without_regex()` | `vim.fn.substitute`, `vim.fn.matchstr` | `E54: regular-expression engine is not installed` |

   Cross-checking each of the 45 against Vimscript splits them cleanly:

   - **24 implemented, unreachable from Lua** — `system`, `systemlist`, `mkdir`, `delete`, `glob`,
     `globpath`, `expand`, `isdirectory`, `filereadable`, `readfile`, `writefile`, `line`, `col`,
     `bufnr`, `bufname`, `win_getid`, `input`, `getchar`, `winheight`, `winwidth`, `cursor`,
     `luaeval`, `execute`, `eval`. Plus every regex builtin, which fails with `E54` rather than
     `E117`. (`strftime` exists but rejects a format string — partial, counted separately.)
   - **20 missing everywhere, Lua and Vimscript alike** — `stdpath`, `getenv`, `environ`,
     `shellescape`, `fnameescape`, `strdisplaywidth`, `localtime`, `reltime`, `reltimefloat`,
     `reltimestr`, `termopen`, `confirm`, `mode`, `visualmode`, `screenrow`, `screencol`,
     `searchcount`, `matchadd`, `sign_define`, `complete`. These need writing, not bridging.

   Counts are over the 45 names probed, chosen because plugins hit them — not over the full builtin
   table.
3. **`vim.cmd` and command/mapping registration.** Of the 165 `vim.api` names oxvim and the oracle
   share, **42 (25%) answer `API function is not implemented`**. The set is not a random tail: it
   includes `nvim_exec2`, `nvim_cmd`, `nvim_exec_lua`, `nvim_create_user_command`,
   `nvim_del_user_command`, `nvim_set_keymap`, `nvim_del_keymap`, `nvim_get_keymap`,
   `nvim_buf_set_keymap`, `nvim_buf_del_keymap`, `nvim_get_mode`, `nvim_set_var`, `nvim_get_var`,
   `nvim_parse_cmd`, `nvim_eval_statusline`, `nvim_set_decoration_provider`. A plugin that cannot
   call `vim.cmd`, define a `:command`, or set a mapping cannot ship a user-visible feature even if
   every module loads. oxvim also exposes 19 fewer `nvim__*` internals, which no plugin needs.
4. **`vim.fn.jobstart` from Lua panics the process.** `crates/oxvim/src/server.rs:1168` routes
   `"jobstart"` into `ExExecutor::call_builtin`, which forwards straight to `call_job_builtin`
   (`crates/ox-editor/src/excmd_exec.rs:655-667`). That function handles `jobstop`, `jobpid`,
   `chansend`, `jobsend` and `jobwait` — **not `jobstart`** — and ends in
   `_ => unreachable!()` at `crates/ox-editor/src/builtins/process.rs:112`:

   ```
   thread 'main' panicked at crates/ox-editor/src/builtins/process.rs:112:14:
   internal error: entered unreachable code
   ```

   The Vimscript path is fine: `crates/ox-editor/src/builtins/process.rs:28-35` has a `"jobstart"`
   arm. Only the Lua route reaches the `unreachable!()`. Ranked below the three above because it is a
   single arm, but it is a crash rather than an error, and async plugins call it constantly.
5. **Treesitter buffer parsing.** `expected either string or buffer handle; buffer parsing is
   unavailable`. Ranked last of the blockers: the parser runtime, queries and node access are real
   and correct, so this is a seam rather than a subsystem.

### Smaller things found on the way

- `--clean` combined with `-u <file>` ignores the file (`server.rs:200` gates on `!cli.clean`); the
  oracle honours the later flag.
- `expand('%:p')` returns the literal string `'%:p'` instead of the empty buffer's path.
- An unknown or unimplemented function aborts the whole script with exit 1 instead of raising a
  catchable exception. `try | let x = nosuchfunc() | catch | ... | endtry` on oxvim exits 1 with
  `oxvim: Ex command failed: not implemented: nosuchfunc` and never reaches the `catch` or the line
  after the `endtry`; the oracle catches `Vim(let):E117: Unknown function: nosuchfunc` and carries
  on. The message wording also differs (`not implemented` vs `Unknown function`). This makes a
  plugin's own capability probes fatal instead of informative.
- `vim.diagnostic` fails to load with `Wrong number of arguments: expecting 2 but got 1` — some API
  called with the wrong arity during module init.

---

## Verdict on the ecosystem premise

**It does not hold today, and the gap is narrower than that sentence suggests.**

Nothing in the plugin ecosystem works right now, and it fails at the very first move: oxvim never
reads the user's config, and even when handed one with `-u`, no plugin module can be required.
A user cannot install lazy.nvim, and lazy.nvim could not load a plugin if they did. On the ladder as
written, the honest score is rung 1a reached, rung 1b failed, and rungs 2 through 5 only reachable
with the rtp loader stubbed out from the probe side.

But what is behind that wall is in much better shape than the wall implies. Once `require` is
force-fed, plenary.nvim — a real, non-trivial library — loads completely and runs its own test
harness with output identical to the oracle. Every module of lazy.nvim, telescope.nvim,
tokyonight.nvim and nvim-treesitter compiles and requires. Buffers, windows, floating windows,
extmarks, namespaces, autocmds, `vim.uv`, `vim.fs`, `vim.iter`, `vim.schedule` and the whole
`vim.tbl_*`/`vim.split` layer behave. oxvim loads a real compiled tree-sitter parser, agrees with
the oracle on ABI 15, and parses and queries source text identically. `'runtimepath'` is already
computed correctly, entry for entry.

So the premise is not refuted — it is unwired. What is missing is concentrated and structural, not a
long tail of behavioural divergence: one rtp search, one `vim.fn` bridge, one group of ~42 API
functions dominated by `vim.cmd` and mapping/command registration, one config-discovery step, one
`unreachable!()`, and ~20 genuinely unwritten builtins. Each is a seam that already has an
implementation on the other side of it. Do the first four and the ecosystem premise moves from
*nothing loads* to *plugins load and mostly run*, which is a far shorter distance than the current
score suggests.
