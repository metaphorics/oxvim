# Oxvim — Greenfield Rewrite

## Goal

Greenfield Rust rewrite of the Neovim core preserving the Lua plugin ecosystem, binary `oxvim`, verified against upstream's functional and oldtest suites.

## What success looks like

The `oxvim` binary answers msgpack-RPC with api_level 15. It runs upstream Neovim's functional and oldtest suites via `NVIM_PRG`, proving behavioral compatibility. It ships a redesigned bundled TUI client speaking only the public RPC protocol.

## Criteria

| # | Criterion | Test command |
|---|-----------|--------------|
| 1 | Workspace builds clean with strict lints | `cargo build --workspace --release` |
| 2 | Unit/property tests pass | `cargo nextest run --workspace` |
| 3 | api-info schema matches upstream | `just apidiff` |
| 4 | Functional suite passes | `just functional` |
| 5 | Oldtests pass | `just oldtest` |

## Out of scope

- Changes to `.references/`
- Publishing to crates.io
- External GUI support beyond the RPC protocol

## Done criterion

- [ ] `cargo build --workspace --release` succeeds with strict lints
- [ ] `cargo nextest run --workspace` passes
- [ ] `just apidiff` reports zero schema differences
- [ ] `just functional` passes
- [ ] `just oldtest` passes
- [ ] no new clippy warnings

## Cleanup

Delete `.agent-tasks/oxvim-greenfield/` after the work merges.