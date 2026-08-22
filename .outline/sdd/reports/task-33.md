# Task 33: runtime color schemes

## Status

Complete. `:colorscheme` now resolves the first matching `colors/<name>.vim` or `colors/<name>.lua` from the Ex executor's ordered runtime roots, sources it through the existing Vimscript/Lua hosts, publishes `g:colors_name`, and executes the planned `ColorScheme` autocmd actions. Missing schemes raise E185 without changing the prior color name or firing the event. Oxvim installs its resolved runtime root into both Ex executors, so the shipped `default` and `vim` schemes load during startup.

## Commit

- `8db3a20 feat(editor): source runtime color schemes`

## Implemented contract

- Runtime lookup preserves root priority and prefers the Vim file when one root contains both Vim and Lua variants.
- Vim source errors and Lua load/runtime errors return before `g:colors_name` or `ColorScheme` dispatch.
- Successful sourcing sets `g:colors_name` before planning `ColorScheme`; Ex and Lua actions execute in definition order, and `++once` actions are consumed when execution begins.
- Lua callbacks receive scope changes, including the new color name and preceding Ex autocmd mutations, through the existing editor/scope synchronization seam.
- `:highlight` retains ordinary key/value definitions plus `default`, `link`, `default link`, forced-link, group clear, and global clear forms in the editor highlight table.
- Unknown schemes raise `E185: Cannot find color scheme '<name>'`.

## Verification

- Focused Ex behavior modules: `cargo nextest run -p ox-editor excmd_exec_` — 181 passed, 0 skipped within the selected modules.
- Required gate: `cargo nextest run -p ox-editor -p oxvim` — 524 passed, 0 skipped.
- Shipped runtime smoke: `target/debug/oxvim --headless -u NONE --cmd "colorscheme default" --cmd "colorscheme vim"` — exited successfully with no error output.
- Review regression: a Lua `ColorScheme` callback observes and preserves the newly assigned `g:colors_name`.

## Oldtest end state

Direct invocation from `.references/neovim/test/old/testdir`:

`/home/alpha/rewrite/Oxvim/target/debug/oxvim -u NONE -i NONE --noplugin --headless --cmd "set shortmess-=F backupdir=. undodir=. viewdir=." -S runtest.vim test_functions.vim`

The harness advances past `colorscheme vim` and exits 1 at the next named blocker, `not implemented: language`. No `.res` is produced.

## Concerns

- `:language` is the next oldtest command blocker.
- Color-scheme lookup supports the runtime's Lua scheme in addition to the contract's Vim path because the shipped `vim` scheme is `runtime/colors/vim.lua`.
