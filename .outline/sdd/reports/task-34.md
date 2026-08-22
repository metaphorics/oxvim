# Task 34: `:language`

## Status

Complete. `:language` now mirrors upstream `os/lang.c` `ex_language` through the C library: the optional `messages`/`ctype`/`time`/`collate` keyword (any unambiguous case-insensitive prefix of at least three characters) selects the category, a bare name sets `LC_ALL`, an empty argument reports the current setting, and a C-rejected locale raises E197. Successful sets apply upstream's environment effects (`$LC_ALL` reset to empty, `LANG`/`LANGUAGE`/`LC_MESSAGES` propagation per category, `LC_NUMERIC` pinned to "C") and republish `v:lang`, `v:ctype`, `v:lc_time`, and `v:collate` from the final locale state. The oldtest harness advances past `lang mess C` and now blocks on the unmodeled `runtimepath` option.

## Commits

- `90f8e0c feat(sys): locale query and mutation through setlocale` — audited `ox_sys::locale` seam (`current_locale`/`set_locale` over `setlocale(3)` with copied static-buffer results and the main-thread exclusion contract mirroring `set_env`); first direct `libc` dependency.
- `63cb593 feat(editor): implement :language` — `command_language` in the Ex executor plus dispatch wiring and behavioral tests.

## Implemented contract

- Keyword parsing reproduces upstream's `skiptowhite` + `STRNICMP(arg, keyword, token_len)` shape: the token must be a whole prefix of the keyword (`mess` matches, `messagesxyz` never does), and tokens shorter than three characters stay locale names so `me` cannot be mistaken for the keyword.
- `:language [keyword]` without a name emits `Current {keyword }language: "{locale}"` as an Echo message, substituting `Unknown` for NULL/empty queries.
- A set that the C library rejects raises `E197: Cannot set language to "{name}"` before any state changes (both `ctype` and `time` forms, as oldtest `Test_language_cmd` exercises).
- Environment effects run only on success and exactly where upstream does them: `LC_ALL=""` always; `LANG=name` and `LANGUAGE=""` only for `LC_ALL`; `LC_MESSAGES=name` for every category except ctype, time, and collate. `LC_NUMERIC` is re-pinned to "C" after each successful set.
- `v:lang`/`v:ctype`/`v:lc_time`/`v:collate` are recomputed from `setlocale` queries after each successful set (upstream `set_lang_var`), with missing queries as empty strings; they sync into `v:` through the executor's existing vim-var path.
- Env writes go through the audited `ox_sys::set_env` and are mirrored into the executor's `$` scope so `$LC_ALL`-style reads observe them.
- Deliberately out of scope, per plan decision 7 and the option model: 'helplang' default seeding (no option table entry), the GNU gettext catalog-counter bump, and `maketitle()` (no title model). None is observable in this port.

## Verification

- New focused tests: `cargo nextest run -p ox-editor language` — 5 passed (abbreviated `lang mess C` env+`v:` effects, `LC_ALL` form, ctype leaving `LANG`/`LC_MESSAGES` untouched, E197 for ctype/time forms, current-locale reporting).
- Required gate: `cargo nextest run -p ox-editor` — 495 passed, 0 skipped.
- `cargo build -p ox-editor` warning-free.

## Oldtest end state

Direct invocation from `.references/neovim/test/old/testdir`:

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

`lang mess C` (runtest.vim:150) now succeeds. The harness exits 1 at the next named blocker, `E355: Unknown option: runtimepath`, from `let &runtimepath ..= ',' .. expand($BUILD_DIR) .. '/runtime/'` (runtest.vim:151). No `.res` is produced. The next task is the `runtimepath` option ('rtp') in the editor option model (and the `&runtimepath ..=` compound assignment path).

## Concerns

- `setlocale` is process-wide libc state; `ox_sys::locale` documents the same main-thread exclusion contract as `set_env`. Unit tests mutate `LC_ALL`/`LANG`/etc. and rely on nextest's per-process test isolation (the repo's existing `setenv` tests already do).
- `v:lang` and friends are still not initialized at editor startup (upstream `set_lang_var` runs after `init_locale`); they exist only after the first `:language`. Oldtest runtest.vim sets `lang mess C` before any test body, so nothing currently reads them earlier, but a fidelity gap remains for scripts evaluating `v:lang` pre-`:language`.
- `language messages`-only sets do not touch `v:ctype`'s env (`LANG`) by design; verified by a sentinel test.
