# Concepts

## winfixbuf guard seam

In this repo, 'winfixbuf' enforcement lives at the Ex-command layer (`winfixbuf_blocks` in `excmd_exec.rs`), never inside the shared `Editor::set_current_buffer` sink, because the sink also serves splits, preview windows, and internal re-entry that must not be blocked; quickfix jumps are the one seam that redirects (previous window, then split) instead of failing.

*Avoid:* guarding `set_current_buffer` itself; guarding the option table.
