---
title: "Working directory state owner in the editor model"
category: architecture
module: ox-editor
date: 2026-08-31
problem_type: logic_error
component: development_workflow
root_cause: scope_issue
resolution_type: code_fix
severity: high
symptoms:
  - "Window-local directory state lost across model transitions and buffer replacement"
  - "Executor-owned previous directory history not shared across interfaces or window scopes"
  - "Direct Editor close_tabpage calls missing automatic directory reapply hook"
  - "New tabpages fail to inherit source window-local directory history"
tags:
  - working-directory
  - window-state
  - editor-model
  - lifecycle-hooks
  - test-isolation
---

# Working directory state owner in the editor model

This document records the diagnosis and fix for working directory state tracking in Oxvim (GitHub issue #23). For the full design decision, see [ADR-001: Own Working Directory State in the Editor Model](../../decisions/ADR-001-editor-owned-working-directory-state.md).

## Problem and reproducible failure

Oxvim did not store working directory state on editor model structures, relying instead on direct operating system process mutations without tracking scope or history.

This caused reproducible failures across transitions:
1. Window-local working directory state was lost across model transitions and buffer replacements in the active window.
2. Previous directory history was owned by executor runtimes rather than editor model structures, preventing history sharing across interfaces and window scopes.
3. Direct model lifecycle operations like `Editor::close_tabpage` lacked automatic directory reapply hooks, leaving the process working directory on the closed tabpage.
4. Newly created tabpages failed to inherit window-local directory and previous directory history from the source window.

## Root cause: why command-level tests masked owner defects

The command execution layer masked missing lifecycle hooks in the model:
- The `:tabclose` command wrapper in `ExExecutor` explicitly called `set_current_tabpage` after closing a tab, synchronizing the destination directory.
- This wrapper masked the defect in `Editor::close_tabpage`, which lacked its own directory reapply hook when called directly.
- Core model structures (`Editor`, `WindowState`) held no working directory or previous directory state fields, so direct model transitions had no model state to synchronize via `std::env::set_current_dir`.

## Fix: editor ownership and cached process directory

We placed state ownership inside `Editor` (global directory and previous directory) and `WindowState` (window-local directory and previous directory).

Key architectural points (see ADR-001 for full specification):
- `Editor::apply_effective_directory()` synchronizes the active window effective directory to the process via `std::env::set_current_dir` as a best-effort cache across model transitions.
- Automatic reapply is silent and best-effort, matching upstream Neovim `update_cwd` (`window.c:5365`): operating system failures never roll back committed model transitions and do not queue `E344`.
- New tabpages and window splits inherit `local_directory` and `previous_directory` from the source window.
- Tabpage-local and buffer-local scopes remain outside this decision and distinct from window-local state.

## Red tests and verification

We verified the fix against focused regression tests covering direct model APIs and command execution:
- `closing_current_local_tabpage_restores_destination_directory`: Verifies that direct `Editor::close_tabpage` calls restore destination tabpage effective directory without Ex wrappers.
- `cd_minus_toggles_and_returns_previous_directory`: Verifies that `:cd -` toggles between the active and previous global directories.
- `windows_maintain_independent_lcd_minus_history`: Verifies that windows maintain independent `:lcd -` previous directory history.
- `window_local_directory_survives_buffer_replacement`: Verifies that window-local directory state survives buffer replacement in the active window.
- `tabnew_inherits_source_window_local_history`: Verifies that new tabpages inherit local directory and previous directory history from the source window.

The focused `excmd_exec_state_tests::` module ran 115 passing tests and package nextest ran 1,426 tests.

## Prevention

To prevent similar state ownership and lifecycle regressions:
- Test model lifecycle methods directly through `Editor` struct APIs in addition to command string execution.
- Maintain state ownership inside core model entities (`Editor`, `WindowState`) rather than transient execution runtimes (`ExRuntime`).
- Ensure every model mutation path that alters the active window invokes the corresponding synchronization hook.
- Verify upstream Neovim source semantics for error handling on automatic transitions before adding custom error queuing.

## References

- GitHub issue: [Issue #23](https://github.com/metaphorics/oxvim/issues/23)
- Architecture decision: [ADR-001: Own Working Directory State in the Editor Model](../../decisions/ADR-001-editor-owned-working-directory-state.md)
