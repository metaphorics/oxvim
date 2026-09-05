# Changelog

## [Unreleased]

### Added
- You can now use `:b`, `:bfirst`, `:blast`, `:brewind`, `:first`, `:last`,
  `:rewind`, `:argument`, `:ex`, `:view`, `:visual`, and `:drop` as Ex commands
  (buffer-switching aliases with `winfixbuf` E1513 guards).
- You now get E742/E1211 errors instead of silent defaults when `setqflist()`
  receives a locked list or non-List items.
- You can now rely on `getcompletion('', 'buffer')` returning `[]` and
  `getcompletion('Foo', 'buffer')` matching basenames upstream-style.

### Changed
- Typing is now cheaper on large buffers: the insert/replace paths snapshot
  only the cursor context instead of copying the whole buffer per keystroke.
- `:resize` now follows upstream semantics: bare maximizes to layout height-1,
  `+/-` forms are relative to the target window's own height, `:Nresize`
  selects the Nth window.
- Unix listen sockets are now created 0600 inside 0700 uid-qualified
  directories; CWD `./runtime` is no longer auto-sourced for rtp/packpath/Lua.
- A failing `vim.schedule` callback no longer kills the editor or the drain;
  the error is reported and draining continues.

### Fixed
- You now get E1513 when buffer-switching commands (`:edit`, `:enew`,
  `:buffer`, `:bnext`, `:next`, `:find`, `gf`, `:tag`, quickfix jumps) target
  another buffer from a `winfixbuf` window; bang and same-buffer cases pass.
- Quickfix jumps from a pinned window now redirect to the previous window or
  split, instead of switching in place.
- Bare `:argadd` now appends the current buffer name whole (no whitespace
  split); unnamed buffers are a silent no-op.
