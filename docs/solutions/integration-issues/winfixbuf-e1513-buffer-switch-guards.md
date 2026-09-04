---
title: "Enforcing 'winfixbuf' E1513 across buffer-switch command seams"
date: 2026-09-05
category: integration-issues
module: ox-editor
problem_type: integration_issue
component: command_execution
symptoms:
  - "test_winfixbuf.vim failed 126 of 144 oldtest cases: E1513 was never raised"
  - "':set winfixbuf' parsed and echoed wfb=1 but ':edit other' switched buffers freely"
  - "':bnext', ':buffer', ':next', ':find', ':tag' and quickfix jumps all ignored the pin"
root_cause: missing_validation
resolution_type: code_fix
severity: high
tags: [winfixbuf, e1513, buffer-switch, upstream-fidelity, ex-commands, quickfix]
related_components: [quickfix, normal-mode, options, arglist, tags]
---

# Enforcing 'winfixbuf' E1513 across buffer-switch command seams

## Problem

The port's generated option table already contained the window-local boolean
`winfixbuf` (built from upstream `options.lua`), so `:set winfixbuf` succeeded
and `&winfixbuf` reported 1 — but nothing enforced it. Every buffer-switching
command silently switched buffers in a pinned window, and
`test_winfixbuf.vim` failed 126/144 upstream oldtest cases.

## Symptoms

- `test_winfixbuf.vim` 126/144 failing (focused oldtest census).
- `:edit other` in a `'winfixbuf'` window switched buffers with no E1513.
- `:bnext`/`:buffer`/`:next`/`:find`/`:tag`/`:cc` all ignored the pin.

## What Didn't Work

- An option-only implementation: the port had the option metadata and value
  plumbing (parse, report, `wfb=1`) with zero call sites consulting it. An
  option that parses but never enforces is silent drift — the census showed it
  as per-test failures, not as an obvious missing feature.
- Guarding the shared `Editor::set_current_buffer`/`set_window_buffer` sink:
  the sink also serves internal switches (splits, preview windows, cmdline
  window, quickfix window re-entry) that must NOT be blocked. Upstream guards
  at the command layer for the same reason.

## Solution

Mirror upstream's two checks (`window.c:199-224`) as one command-layer helper
and call it at every seam upstream calls `check_can_set_curbuf_forceit` /
`check_can_set_curbuf_disabled`:

```rust
// excmd_exec.rs — returns Some(E1513 flow) unless the bang overrides.
fn winfixbuf_blocks(runtime: &ExRuntime<F>, editor: &Editor, forceit: bool)
    -> Option<Flow>
{
    if forceit || !editor.current_window_fixed_to_buffer() {
        return None;
    }
    Some(error_flow(runtime, "E1513",
        "Cannot switch buffer. 'winfixbuf' is enabled"))
}
```

`Editor::current_window_fixed_to_buffer()` (editor.rs) reads the window-local
option — the port's `curwin->w_p_wfb`.

Guarded seams, each with upstream's exact escape conditions:

| Port seam | Upstream | Fires when |
|---|---|---|
| `command_edit` (+ `:ex`/`:visual`/`:view`/`:drop` aliases) | `do_ecmd`, ex_docmd.c:5987 | target name differs from current buffer (`is_other_file`); bare `:edit` reload is exempt |
| `command_enew` | ex_docmd.c:5987 | always (new buffer); `enew!` overrides |
| `command_buffer` / `command_buffer_step` / `command_buffer_absolute` | `do_buffer`, buffer.c:1396 | target handle != current; bang overrides |
| `edit_argument_file` (`:next`/`:previous`/`:first`/`:last`/`:argument`/`:wnext`/`:wprevious`) | `do_argfile`, arglist.c:619 | entry resolves to a different buffer; bang overrides |
| `command_find` | `ex_find`, ex_docmd.c:5941 | always; `find!` overrides |
| `open_tag_buffer` in-place tail | `do_tag`/`jumpto_tag`, tag.c:308/2633 | `postponed_split == 0` equivalent: `!split && target != current` |
| `goto_file_under_cursor` (`gf`) | `nv_gotofile`, normal.c:3871 | always — `_disabled` check, NO bang escape |
| `quickfix::jump` | `qf_jump_edit_buffer`, quickfix.c:2969-3006 | prevwin-then-split dance (below) |

Quickfix does not fail like the others — it redirects:

```rust
// quickfix.rs jump(): E1513 only when neither escape works.
// 1) go to previous window if it is not pinned and not the qf window
// 2) else split a fresh (unpinned) window below
// 3) else E1513
```

Location-list entries cannot reassign their window upstream and fail
immediately; the port shares the qf path until loclist routing exists.

## Why This Works

Guards live at the same architectural seam as upstream — between Ex command
dispatch and the shared buffer-switch sink — with upstream's same-file escape
(a `:edit` of the buffer's own name is a reload, not a switch) and bang
semantics (`forceit` from `:edit!`, `:b!`, `:next!`...). The sink stays
unguarded, so splits, preview windows and internal re-entry keep working.
Verified: focused oldtest `test_winfixbuf` 126 → 109 failing (remainder is
missing-command E117s: `:bufdo`/`:cdo`/`:ldo`/`:cfile`/`:vimgrep` family);
`cargo nextest --workspace` 2985 passed / 1 skipped.

## Prevention

When porting an upstream option, grep upstream for its enforcement call sites
before wiring the value plumbing: `grep -rn "w_p_wfb\|check_can_set_curbuf"
.references/neovim/src/nvim` mapped every seam above in one pass. An option
that parses but never enforces shows up only as scattered per-test failures —
the census bucket names the file, and the upstream call-site map names the
seams.
