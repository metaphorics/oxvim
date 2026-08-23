# Task 77: the runtime-path and API seams

Status: **items 1 and 2 done; item 3 partly done, partly blocked on `ox-editor`.**
Crates touched: `crates/ox-api/`, `crates/ox-lua/` only.

Oracle: `.references/neovim/build/bin/nvim`, `v0.13.0-dev-1390`, API level 15, run with
`VIMRUNTIME=.references/neovim/runtime` so both binaries have a runtime tree. Every probe used a
throwaway `HOME` and `XDG_*` under `/tmp/t77probe/<name>/home`; no probe saw a real home or config,
and nothing was written inside `.references`.

## Commits

| sha | subject |
| --- | --- |
| `8a5c552` | feat(ox-api): search runtime files over 'runtimepath' |
| `1d7cfc7` | feat(ox-api): merge option values for vim.opt append, prepend and remove |
| `8d6c24c` | feat(ox-api): register mappings, and run Ex and Lua through host seams |
| `7f03de7` | test(ox-api): pin both runtime-path dedup guards, not just one |
| `4998cee` | fix(ox-lua): default an omitted trailing optional Dict argument |

## 1. Runtime-file search over 'runtimepath'

`RuntimeState::runtime_paths` is gone. The search path is now derived from the editor's
'runtimepath' and 'packpath' on demand, following runtime.c `runtime_search_path_build`: walk
'runtimepath' until the first `after` entry, expanding each entry's wildcards and splicing in the
start bundles of any entry that is also a 'packpath' entry; then the bundles of the remaining
'packpath' entries; then every queued package `after` directory; then the rest of 'runtimepath'
from where the first pass stopped. The first occurrence of a directory wins. The result is cached
against the two option strings it was built from, as upstream caches it behind
`runtime_search_path_valid`.

`FileIO` was rebuilt around the operations the search actually needs — a component-wise wildcard
expansion, an `is_dir` probe and an `is_readable` probe — replacing a recursive walk of the whole
tree per lookup. `nvim__get_runtime` moved out of ox-lua's embedded preloader into `bind_api`,
where the editor whose 'runtimepath' it must walk is in scope; it stays out of the registry
because upstream keeps it out of the canonical API metadata, which the registry is built from.

Oracle comparison, same fixture tree for both binaries:

| probe | before | after | oracle |
| --- | --- | --- | --- |
| `nvim_list_runtime_paths()` | `{}` | the existing rtp directories, in order | same |
| `nvim_get_runtime_file('lua/vim/fs.lua', true)` | `{}` | the file | same |
| `nvim__get_runtime({'lua/vim/fs.lua'}, false, {is_lua=true})` | `{}` | the file | same |
| `require` of a module on the rtp | not found | loads | loads |

## 2. `vim.opt` append, prepend, remove

`nvim_set_option_value` rejected all three. It now performs upstream's merge (option.c
`get_option_newval`): numbers add, multiply and subtract; strings follow `stropt_get_newval`,
including the no-duplicate rule, single-separator removal, the `key:value` keymatch path and
flag-list deduplication. Options flagged `expand` substitute `$VAR`, `${VAR}` and a `~` that opens
the value or a list item — which is what makes `vim.opt.rtp:prepend('~/x')` store an absolute path
— and that expansion applies to a plain `set` too, as upstream's does.

Every case below was read off the oracle first and then matched:

| call | oracle and oxvim |
| --- | --- |
| `rtp = '/a,/b'`, append `/c` | `/a,/b,/c` |
| prepend `/z` | `/z,/a,/b,/c` |
| remove `/b` | `/z,/a,/c` |
| append an entry already present | unchanged |
| remove an entry that is absent | unchanged |
| `scrolloff = 5`, append / prepend / remove `3` | `8` / `15` / `2` |
| `shortmess = 'filnx'`, append `tI` then remove `l` | `filnxtI` then `finxtI` |
| `fillchars = 'vert:|,fold:-'`, append `fold:.` | `vert:|,fold:.` |
| prepend `~/tp` | `$HOME/tp,/a` |
| unknown operation | `Invalid 'operation': expected 'set', 'append', 'prepend', or 'remove'` |
| append to a boolean option | `Conflict: 'append' not allowed with boolean options` |

## 3. The API names blocking plugin bootstrap

**Landed and oracle-checked:** `nvim_set_keymap`, `nvim_del_keymap`, `nvim_get_keymap`,
`nvim_buf_set_keymap`, `nvim_buf_del_keymap`, `nvim_buf_get_keymap`. Every field of the listing
dict and every rejection message matches the oracle, `sid` excepted — an API-created mapping there
carries the calling channel's script id, which this port does not track. Abbreviation modes
(`ia`, `ca`, `!a`) are refused rather than half-stored: this port keeps abbreviations without the
mode set, flags and description an entry would have to report. `vim.keymap.set` with a Lua
callback round-trips with the oracle's key set, `callback` included.

**Landed, awaiting a one-line install in `crates/oxvim`:** `nvim_exec2`, `nvim_cmd`,
`nvim_command`, `nvim_exec_lua`. Executing Vimscript and Lua belongs to the embedder, so these run
through two new seams on the API's runtime state, `ox_api::set_command_executor` and
`ox_api::set_lua_executor`. Both are exercised end to end here through test hosts. Until
`Server::new` installs the real ones they report that no host is installed rather than that the
function does not exist. The owner of `crates/oxvim` has the exact signatures and declined the
scope mid-brief; this is the one thing standing between plenary's harness and a clean exit.

**Blocked on `ox-editor`:** `nvim_create_user_command`, `nvim_del_user_command`,
`nvim_get_commands`. The global user-command registry is `ExRuntime::user_commands`, crate-private,
and command resolution runs through `parse_program` against it. Exposing an editor-level map means
moving resolution — a refactor the owner of that crate declined mid-brief. A parallel store in
ox-api would define commands that `:MyCmd` could never find, so nothing was written.

**Still unimplemented, with what each needs** (40 `nvim_*` names; the 67 legacy `vim_*`/`buffer_*`/
`window_*`/`tabpage_*`/`ui_*` aliases are excluded):

- *Global variables* — `nvim_set_var`, `nvim_get_var`, `nvim_del_var`. `g:` lives in the Ex
  executor's `Scope`, not on `Editor`; needs the same kind of editor-level store as user commands.
- *User commands* — `nvim_create_user_command`, `nvim_del_user_command`, `nvim_get_commands`,
  `nvim_buf_del_user_command`, `nvim_buf_get_commands`. As above.
- *Marks* — `nvim_get_mark`, `nvim_del_mark`, `nvim_buf_get_mark`, `nvim_buf_set_mark`,
  `nvim_buf_del_mark`. `ox_editor::marks` is public; these are implementable in ox-api as-is.
- *Eval* — `nvim_eval_statusline`, `nvim_parse_expression`, `nvim_call_dict_function`.
  `nvim_parse_expression` needs `ox_eval`'s parser to expose its AST; the other two need the
  stateful evaluator the `vim.fn` bridge now has.
- *Command introspection* — `nvim_parse_cmd`. `ox-excmd`'s parser is already reachable from ox-api;
  this is a decoding job with no new seam.
- *Options introspection* — `nvim_get_all_options_info`, `nvim_get_option_info2`. Pure `OptionStore`
  metadata, implementable in ox-api as-is.
- *Colors* — `nvim_get_color_map`, `nvim_get_color_by_name`. Needs the name-to-RGB table; a sibling
  task is probing exactly that.
- *Current line* — `nvim_get_current_line`, `nvim_set_current_line`, `nvim_del_current_line`.
  Buffer plus cursor, both available; implementable in ox-api as-is.
- *Windows and tabs* — `nvim_win_resize`, `nvim_win_text_height`, `nvim_open_tabpage`.
  `nvim_win_text_height` needs wrapped-line measurement from the layout engine.
- *Process and cwd* — `nvim_get_proc`, `nvim_get_proc_children`, `nvim_set_current_dir`,
  `nvim_get_mode`, `nvim_input_mouse`.
- *UI protocol* — `nvim_ui_pum_set_bounds`, `nvim_ui_pum_set_height`, `nvim_ui_send`,
  `nvim_ui_set_focus`, `nvim_ui_set_option`, `nvim_ui_term_event`, `nvim_ui_try_resize_grid`,
  `nvim_set_decoration_provider`. UI-attached surfaces owned by the TUI work.

One defect was found and fixed on the way: the Lua bindings appended a default options Dict for
exactly two function names, so `vim.cmd(...)` — which calls `nvim_exec2(src)` with one argument —
answered "Wrong number of arguments: expecting 2 but got 1". Padding now comes from the registry
metadata, which already records which parameters are optional.

## Plugin evidence, with no client-side shim

`vim.opt.rtp:prepend(...)` — which needs item 2 — followed by ordinary `require`, which needs
item 1. Nothing is stubbed from the Lua side.

**plenary.nvim** (`74b06c6`). `plenary`, `plenary.path`, `plenary.job`, `plenary.busted`,
`plenary.async`, `plenary.filetype` and `plenary.strings` all load, and `plenary.collections`
fails on both binaries because there is no such module. `Path:new(dir):exists()` and
`:absolute()` agree with the oracle. Its `busted` harness runs a real spec file and produces
output byte-identical to the oracle's:

```
========================================
Testing: 	/tmp/t77probe/spec/spec_test.lua
Success	||	probe adds
Success	||	probe uses vim api

Success: 	2
Failed : 	0
Errors : 	0
========================================
```

The harness's own exit `qa!` still fails, now with `no Ex-command host is installed` rather than
`API function is not implemented` — the install described in item 3.

**lazy.nvim** (`306a055`), the documented six-step bootstrap, both binaries pointed at the same
clone so the comparison is about the code and not the fixture:

```
                            oxvim                       oracle
1 stdpath(data)             FAIL E117: stdpath          OK
2 uv fs_stat(lazypath)      OK   true                   OK   true
3 vim.fn.system             OK   git version 2.53.0     OK   git version 2.53.0
4 vim.opt.rtp:prepend       OK   /tmp/.../lazy.nvim     OK   /tmp/.../lazy.nvim
5 require("lazy")           OK   table                  OK   table
5   lazy.core.util          OK   table                  OK   table
5   lazy.core.config        FAIL E117: stdpath          OK   table
6 lazy.setup({})            FAIL E117: stdpath          FAIL (lazy/nightly mismatch)
```

Before this task lazy.nvim failed at step 1 of 6 and reached nothing. Steps 2 through 5 now pass,
and the single remaining wall is `vim.fn.stdpath`, which belongs to the builtin work in a sibling
crate. Step 6 fails on the oracle too, on an unrelated lazy.nvim/nightly incompatibility.

## Tests

| suite | before (`2ad1975`) | after |
| --- | --- | --- |
| `cargo test -p ox-api -p ox-lua -- --test-threads=1` | 118 passed, 0 failed | **137 passed, 0 failed** |
| workspace, `--no-fail-fast -- --test-threads=1` | 2004 | **2049 passed, 0 failed** |

One test moved rather than disappeared: ox-lua's `runtime_listing_uses_the_supplied_root_and_all_flag`
covered the single-root `nvim__get_runtime`, which no longer exists; its subject is now eight
ordering tests in ox-api.

Two `tests/differential` tests failed mid-run and are **not** from this work: a release binary built
from committed `HEAD` passes both, and the failures only appeared while a sibling's uncommitted
startup plugin-sourcing was in the shared tree, where `runtime/plugin/gzip.vim` aborted startup on a
comma-separated `:autocmd` event list. That was reported to the owner and is fixed on `main` as of
`97c15ce`; `cargo test -p differential` is green again.

## Mutation check

The runtime search is an ordering rule with several independent parts, so each was deleted
separately and the suite re-run. Nine mutations, nine killed:

| mutation | killed by |
| --- | --- |
| never stop early in `find_runtime_files` | `runtime_file_lookup_honors_the_all_flag`, `..._follows_runtimepath_order` |
| never stop early in `runtime_get_named` | `runtime_file_lookup_honors_the_all_flag`, `lua_lookup_probes_literal_paths_in_order` |
| sort the search path by path text | 5 tests, including `after_entries_keep_their_runtimepath_position` |
| partition `after` entries to the end instead of resuming in rtp order | `after_entries_keep_their_runtimepath_position` |
| place a package's `after` dir beside its bundle | `package_bundles_follow_their_packpath_entry_and_after_dirs_come_last` |
| drop the first-occurrence-wins guard | `wildcard_entries_expand_and_repeats_collapse` |
| treat every rtp entry as a literal directory | `wildcard_entries_...`, `package_bundles_...` |
| skip the start-bundle splice | `package_bundles_...` |
| never skip an entry without a `lua/` directory | `lua_lookup_skips_entries_without_a_lua_directory` |

The dedup mutation **survived the first round**, and that is the useful result. The test named a
directory that a wildcard entry had already placed, which runtime.c drops at the entry check before
it expands anything — so the per-expansion check was never exercised and deleting it left the suite
green. `7f03de7` adds the two overlapping-wildcard orders (`/w/*,/c,/w/p1*` and `/w/p1*,/w/*`) that
only the second guard can answer, and the mutation now fails.

The `after` pair is the same idea: `/a,/b/after,/c` and `/a,/c,/b/after` are separate tests
precisely because a naive "non-after entries first, after entries last" implementation gives the
same answer for both, and the oracle does not.

## Concerns

- **`vim.cmd` is one call away and it is not mine to make.** `nvim_exec2`/`nvim_cmd`/`nvim_command`/
  `nvim_exec_lua` are complete and tested but inert until `crates/oxvim` installs the two hosts. A
  default host inside ox-api would need its own `ExExecutor`, whose script scope and `g:` variables
  would not be the ones Vimscript sees, so it would silently shadow state — worse than the honest
  error. This should be sequenced explicitly rather than assumed.
- **`nvim_exec_lua` re-enters.** The Lua bindings hold a mutable borrow of the editor when Lua calls
  `vim.api`, and the chunk the host runs will call `vim.api` again. The seam hands the host an
  `&mut Editor` so it need not take a second borrow, but an adapter that ignores that and borrows
  its own `Rc<RefCell<Editor>>` will panic. Same hazard the `:lua` path already carries.
- **`sid` on mappings is 0, not the channel's script id.** Everything else in the listing matches.
  A plugin that reads `sid` to attribute a mapping will see zero.
- **The search path cache keys on the option strings, not the filesystem.** Creating a directory
  that an existing wildcard entry would match is not noticed until 'runtimepath' or 'packpath'
  changes. Upstream has the same class of staleness, and every path that installs a plugin also
  touches the rtp, but `:!mkdir` followed by `:runtime` will not see the new directory.
- **Expansion now applies to a plain `set` of an expand-flagged option**, which it did not before.
  That is upstream's behaviour and the suite is green, but it is a behaviour change beyond the three
  merge operations the brief named.
- **Startup now sources `runtime/plugin/*.vim` for real**, because discovery works. That surfaced
  four separate unimplemented things in oxvim's own runtime files (`packadd`, `v:argv`,
  `cpo` literal default, and a comma-separated `:autocmd` event list). They are reported at startup
  rather than fatal, but every one of them is a real gap the rtp fix made visible.
