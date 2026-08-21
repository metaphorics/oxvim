# Oxvim

Oxvim is a greenfield Rust rewrite of the Neovim core that preserves the Lua plugin ecosystem. The binary is `oxvim`; crates are prefixed `ox-`. Upstream Neovim at `.references/neovim/` is the read-only executable specification.

## Commands

- `just build`
- `just test`
- `just functional`
- `just oldtest`
- `just apidiff`

Per-task goals and acceptance tests live in `.agent-tasks/<task-id>/GOALS.md`; they are task-ephemeral and never merge into this file.

Note: `.references/` is read-only; never edit it.