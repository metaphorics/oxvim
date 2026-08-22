# Task 16a — API metadata parity

## Status

Complete. Oxvim's `--api-info` output and `nvim_get_api_info` metadata now match the Neovim 0.13.0-dev API-level-15 oracle exactly after the differential harness's existing `version.build` allowance. The core replay's former API-info sanction is no longer used: `core.yaml` passes without a sanctioned divergence.

## Implementation

- Captured the oracle's complete MessagePack metadata as `crates/ox-rpc/src/api_metadata.msgpack` and exposed a single checked-in canonical decoder through `ox_rpc::canonical_metadata`. This preserves exact top-level key presence, version identity, 262 function dictionaries, all parameter optionality booleans, UI events/options, error types, and handle types. The oracle emits no `custom_events` key, so Oxvim's canonical result omits it.
- Routed both `crates/oxvim/src/api_info.rs` (`--api-info`) and `ox_api::nvim_get_api_info` through that canonical payload. The RPC server continues to replace the anonymous channel placeholder with the requesting channel ID.
- Added channel-aware `nvim_call_atomic` dispatch so nested and recursively nested `nvim_get_api_info` calls also receive the actual RPC channel rather than channel 0. The embedded stdio smoke test pins this behavior.
- Generated `crates/ox-api/src/api_function_names.rs` from the same oracle inventory. `Registry::core` now reconstructs the public registry in canonical order, associates existing native handlers by name, and supplies a typed `ApiError` dispatch for genuinely unavailable functions.
- Extended `FunctionMetadata` with exact raw type expressions, parameter optionality, and `textlock_allow`. Generated descriptors preserve all names, since/deprecation levels, methods, return/parameter types, optionality, and source-declared execution flags. The whole-inventory test compares every emitted descriptor field against the canonical asset and pins the source-derived fast/textlock inventories.
- Removed `nvim__stats`, which is not present in this oracle's level-15 public inventory.

The sanctioned edit to `crates/oxvim/src/api_info.rs` was necessary to make the executable's `--api-info` path consume the canonical `ox-rpc` metadata source; no other worker owned that seam.

## Commit

- `aa530e2 fix(api): match Neovim metadata exactly`

## Verification

- `cargo nextest run -p ox-api -p ox-rpc`: **77 passed, 0 skipped**.
- `cargo nextest run -p oxvim --test smoke`: **8 passed, 0 skipped**; includes direct and atomic nested API-info channel IDs.
- `cargo build --release -p oxvim && just apidiff`: **pass** — `apidiff: schemas match (version.build ignored)`.
- `just differential`: **7 passed, 0 skipped**.
- `just replay`: **pass**. `core.yaml` and `eval.yaml` match exactly; the pre-existing channel, option-return, and UI ordering sanctions remain unchanged.
- `git diff --check`: pass before commit.

## Review

Three focused review rounds found and drove fixes for: placeholder fallback metadata, missing source-declared execution flags, and channel 0 leaking through `nvim_call_atomic`. No verification assets, replay fingerprints, oracle sources, or differential comparator rules were changed.

## Concerns

- The canonical MessagePack asset and generated Rust descriptor table must be regenerated together when the pinned Neovim oracle changes; the whole-inventory tests detect descriptor drift within the checked-in pair, while `just apidiff` detects oracle drift.
- The remaining replay sanctions are outside Task 16a: terminal-channel backing, option setter return values, and initial UI redraw ordering/richness.
