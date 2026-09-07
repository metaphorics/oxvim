# Oxvim agent contract

- Treat `.references/neovim/` as the read-only executable specification. Never edit it; compare observable behavior against it.
- Read `.agent-tasks/<task-id>/GOALS.md` before you change behavior. The task file owns acceptance criteria; delete its task directory after the work merges.
- Preserve Neovim API level 15, existing Lua plugins, exact external errors, callback order, and the single-main-thread editor model. A cleaner implementation does not excuse behavior drift.
- Preserve synchronous reentry at arbitrary depth and keep runtime state isolated per editor session. End every runtime-state borrow before Lua, callbacks, or other user code can reenter.
- Add a crate only when it owns a real dependency, build target, or unsafe seam. Split large files by ownership or an independently changing decision, never by size.
- Derive compatibility inventories from one canonical source. Keep deprecated and unavailable Neovim entries until the upstream API removes them.
- Measure a representative plugin workload before a performance edit and attribute every claimed latency or allocation improvement. Source shape is not evidence.