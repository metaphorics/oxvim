# Task 32: filesystem builtin family

## Status

Complete. Filesystem-dependent Vimscript builtins now execute through the injected `FileIO` host; the typval-only `ox-eval` dispatcher no longer performs direct filesystem predicate or glob operations. The oldtest runner advances past `mkdir()` and `isdirectory()` and stops at the next named command blocker, `not implemented: colorscheme`.

## Commits

- `d2e2134 feat(editor): route filesystem builtins through FileIO`
- `9aa7e0f refactor(eval): host filesystem builtins behind FileIO`
- `e2eefe0 chore(eval): remove obsolete filesystem path helper`

## Implemented contract

- Metadata: `filereadable()`, `isdirectory()`, `getftime()`, `getfsize()`, `getfperm()`, and `filewritable()` return the upstream file/directory distinctions and missing-path sentinels.
- Mutation: `mkdir()` supports ordinary creation, recursive `"p"`, and an optional protection mode; `delete()` distinguishes file, empty-directory `"d"`, and recursive `"rf"`; `rename()` returns `0`/`-1`.
- Content: `readfile()` implements text/binary line behavior, BOM/CR/NUL handling, positive and negative maximum line counts; `writefile()` supports List, String, and Blob data, exact byte writes, text/binary newline rules, append mode, invalid flags, and open/permission failures.
- Discovery: `glob()` and `globpath()` return newline-separated strings or Lists, sort and deduplicate matches, support `*`, `?`, bracket classes, recursive `**`, leading `~`, hidden-file rules, and `alllinks` handling.
- `FileIO` now exposes byte IO, metadata, directory enumeration, creation, removal, and rename operations. Existing test doubles retain explicit unsupported defaults rather than silently touching the real filesystem.

## Upstream citations

Implementation follows `.references/neovim/src/nvim/eval/fs.c`: `f_delete` lines 438-470; `f_filereadable`/`f_filewritable` lines 527-539; `f_getfperm`/`f_getfsize`/`f_getftime` lines 834-887; `f_glob`/`f_globpath` lines 924-1014; `f_isdirectory`/`f_mkdir` lines 1082-1140; `read_file_or_blob` lines 1299-1496; `f_rename` lines 1512-1521; `write_list` and `f_writefile` lines 1714-1760 and 1802-1906. User-facing contracts are corroborated by `.references/neovim/runtime/doc/vimfn.txt`: `delete()` lines 1622-1648, file predicates lines 2444-2490, metadata lines 3826-3870, `globpath()` lines 4872-4909, `mkdir()` lines 7265-7304, `readfile()` lines 8301-8344, and `writefile()` lines 13108-13164.

## Verification

- Focused filesystem module: 6 passed.
- Required gate: `cargo nextest run -p ox-eval -p ox-editor` — 845 passed, 0 skipped.
- Warning-free integration build: `cargo build -p oxvim` completed.
- Direct oldtest invocation: `/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim` exits 1 with `not implemented: colorscheme`; no `.res` is produced.

## Oldtest end state

`setup.vim:116-117` now creates `XfakeHOME` through `mkdir()` after checking it through `isdirectory()`. Startup proceeds to `colorscheme vim`, whose `:colorscheme` Ex command is not implemented. This is the first named blocker after the filesystem change.

## Concerns

- Unix permission predicates use permission mode bits. They match the ordinary oldtest fixtures but do not model ACLs or every effective-user/group edge case.
- Deferred `mkdir()` flags `D`/`R` and deferred `writefile()` flag `D` are outside this task's requested `p`/protection and append/binary contract.
