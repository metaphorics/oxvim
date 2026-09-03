---
date: 2026-08-31
topic: whole-crates-minimalism-architecture-overhaul
focus: offensive structural reduction with measured performance
---

# Ideation: whole-crates minimalism architecture overhaul

Oxvim needs fewer owners, fewer hidden state paths, and fewer mirrored truths. The rewrite must preserve Neovim API level 15, Lua plugin behavior, arbitrary-depth callback reentry, exact observable errors, and the single-writer editor model. Source shape alone does not justify a performance rewrite.

## Grounding context

The workspace has 16 Rust crates. Its dependency graph permits `ox-api` to own explicit session runtime state beside `Editor`; no reverse `ox-editor -> ox-api` edge is required (`crates/ox-api/Cargo.toml`, `crates/ox-editor/Cargo.toml`).

The pre-refactor API runtime combined editor-adjacent state, UI state, channels, file access, caller frames, and three tiers of host objects in one ID-keyed thread-local map. `Editor` carried an API instance ID for that lookup, with no matching removal path (`crates/ox-api/src/runtime.rs:535-642`, `crates/ox-editor/src/editor.rs:232,363,596`). Host checkout then removed, forked, and restored primary, nested, or prototype hosts to support reentry (`crates/ox-api/src/runtime.rs:781-1053`). **Landed**: `ApiSession` (`crates/ox-api/src/session.rs:148-151`) now owns `Rc<RefCell<Editor>>` directly with `SessionState` holding session-owned host pools (`session.rs:71-92`); the thread-local map and editor API instance ID are gone (no `api_instance`/`api_id` in `editor.rs`; `runtime.rs` is now 930 lines). See `.outline/evidence/refactor-docs-sync.md` for the full stale-claim ledger.

Two safety defects are narrower and already concrete. `LoopAccess` can retain stale pointer and flag state when a callback unwinds, even though the outer loop catches the panic and continues (`crates/ox-lua/src/uv_handles.rs:91-175`). Persistent network callbacks can also disappear after a contained panic between take and restore (`crates/ox-uv/src/net.rs:326-369`, `crates/ox-uv/src/uv_loop.rs:573-629`).

The text path contains broad copy points, but not a proven hot path. `Buffer::line` and `to_bytes` materialize Ropey slices, and two editor helpers copy every line into `Vec<Vec<u8>>` (`crates/ox-text/src/buffer.rs:149-163`, `crates/ox-editor/src/editor.rs:3676-3681`, `crates/ox-editor/src/excmd_exec.rs:14902-14908`). Ropey lines can span chunks, so a borrowed string interface would not be generally zero-copy.

Compatibility truth is split. A checked-in API-level-15 MessagePack object exists, while a generated Rust function inventory is maintained separately. Registry assembly overlays 176 implemented functions and 86 unavailable entries across the canonical 262 names (`crates/ox-rpc/src/metadata.rs:33-40`, `crates/ox-api/src/api_function_names.rs:1-4`, `crates/ox-api/src/registry.rs:115-145`).

The dirty current-directory change remains a prerequisite. It does not preserve window-local directory state across buffer and window transitions, can mutate process cwd before returning E16, and lacks a fork-path regression test. A broad runtime rewrite must not bury that red behavior.

## Topic axes

- State ownership and composition
- Execution locality
- Plugin and FFI safety
- Text and rendering performance
- Compatibility truth and tests

## Ranked ideas

- [Explicit session runtime and reentrant host pool](#1-explicit-session-runtime-and-reentrant-host-pool)
- [Unwind-safe active-loop capability](#2-unwind-safe-active-loop-capability)
- [Panic-safe persistent callback slot](#3-panic-safe-persistent-callback-slot)
- [Canonical MessagePack API inventory](#4-canonical-messagepack-api-inventory)
- [Stage-attribution matrix](#5-stage-attribution-matrix)
- [Rope fragmentation and access experiment](#6-rope-fragmentation-and-access-experiment)
- [Dispatch runtime and code-generation experiment](#7-dispatch-runtime-and-code-generation-experiment)
- [Delete the test-side builtin parser clone](#8-delete-the-test-side-builtin-parser-clone)
- [Replace production test seams with observable behavior](#9-replace-production-test-seams-with-observable-behavior)

### 1. Explicit session runtime and reentrant host pool

- Description: Let the session root own one `ox_api::Runtime` beside one live `Editor`. Pass both through every RPC, Lua, Ex, autocmd, UI, and direct API call. Replace primary/nested/prototype tiers with a private pool of returned concrete hosts. Keep role behavior separate, but share one private server composition context.
- Axis: State ownership and composition
- Basis: Direct: `crates/ox-api/src/runtime.rs:535-642,781-1053` (pre-refactor line ranges; file is now 930 lines — see `session.rs` for the landed pools); `crates/oxvim/src/server.rs:2332-2435,2533-2644`.
- Rationale: This removes the foreign editor ID, the thread-local locator, leaked map entries, arbitrary warm tiers, repeated nested setters, and repeated six-field server adapters without adding a crate or umbrella trait.
- Downsides: This is a broad migration. A runtime borrow held across Lua or user code would break reentry. Strong back-references could replace the TLS leak with a cycle. Pool reset rules must preserve every transient host invariant.
- Confidence: 90%
- Complexity: High

### 2. Unwind-safe active-loop capability

- Description: Use exact-prior-state RAII frames for the active loop pointer, callback state, and deferred-drain state. Keep one private closure-scoped unsafe dereference that cannot return a loop reference.
- Axis: Plugin and FFI safety
- Basis: Direct: `crates/ox-lua/src/uv_handles.rs:91-175`.
- Rationale: A reachable panic currently skips restoration. The narrow repair removes that defect while retaining synchronous LIFO reentry.
- Downsides: Restoration alone does not prove alias provenance. The design survives only if normal and nested paths pass Miri under the default model and Tree Borrows.
- Confidence: 96%
- Complexity: Medium

### 3. Panic-safe persistent callback slot

- Description: Give persistent callbacks one internal state owner with installed, invoking, replaced, removed, and closed states. Restore after unwind only when callback code did not remove or replace the slot. Keep one-shot close callbacks separate.
- Axis: Plugin and FFI safety
- Basis: Direct: `crates/ox-uv/src/net.rs:326-369`; `crates/ox-uv/src/uv_loop.rs:573-629`.
- Rationale: This replaces repeated take/call/restore protocols and prevents outer panic containment from silently deleting network behavior.
- Downsides: A simple drop guard is wrong. It can resurrect a callback that removed itself or overwrite its replacement. The state owner needs generation-aware restoration.
- Confidence: 92%
- Complexity: Medium

### 4. Canonical MessagePack API inventory

- Description: Make the checked-in API-level-15 MessagePack object the ordered compatibility inventory. Deterministic offline code generation derives the Rust `API_FUNCTIONS` table. Implementation availability remains a separate overlay.
- Axis: Compatibility truth and tests
- Basis: Direct: `crates/ox-rpc/src/metadata.rs:6-10,33-40`; `crates/ox-api/src/api_function_names.rs:1-4`; `crates/ox-api/src/registry.rs:115-145,209-230`.
- Rationale: One source owns public names, ordering, types, method flags, and deprecation metadata. The registry still distinguishes implemented and unavailable methods.
- Downsides: A generator and comparator can share the same decoding bug. Generation therefore needs deterministic bytes plus an independent observable API-info check.
- Confidence: 92%
- Complexity: Medium

### 5. Stage-attribution matrix

- Description: Measure end-to-end latency and internal CPU and allocation costs across startup, RPC decode, text topology, extmarks, regex, builtins, open, edit, scroll, and render stages. Include negative controls and identical correctness snapshots.
- Axis: Text and rendering performance
- Basis: Direct absence of internal attribution: `tests/differential/src/perf/session.rs:205-221,738-804`.
- Rationale: This rejects optimizations chosen because code looks large. Only a stage that materially affects end-to-end work earns a rewrite.
- Downsides: Instrumentation can perturb allocations and branches. Synthetic workloads can charge decoder or process startup cost to the wrong stage.
- Confidence: 97%
- Complexity: Medium

### 6. Rope fragmentation and access experiment

- Description: Compare byte-identical contiguous and edit-fragmented buffers. Measure `RopeSlice::as_str()` hits, chunks per line and range, copied bytes, allocations, CPU time, and sequential, random, and retained access.
- Axis: Text and rendering performance
- Basis: Direct: `crates/ox-text/src/buffer.rs:149-163`; `crates/ox-editor/src/editor.rs:3456-3460`; `crates/ox-editor/src/excmd_exec.rs:14400-14405`.
- Rationale: The experiment can reject a broad borrowed-text interface before lifetime machinery spreads through callers.
- Downsides: Equivalent fragmentation histories are hard to construct. A chunk-aware iterator can still move the copy into a regex or protocol sink instead of removing it.
- Confidence: 95%
- Complexity: Low

### 7. Dispatch runtime and code-generation experiment

- Description: Attribute builtin specification lookup, implementation selection, Vimscript builtin dispatch, and Ex dispatch on real plugin and builtin-heavy workloads. Compare an alternative representation only if dispatch is material; include clean-build time and binary size.
- Axis: Execution locality
- Basis: Direct: `crates/ox-eval/src/builtins.rs:130-142,192-205,270-340`; `tests/differential/src/perf/session.rs:205-221`.
- Rationale: The result can reject perfect hashing, generated handler tables, trait-object dispatch, or forced inlining. A surviving representation must remove separate truths rather than add another table.
- Downsides: Inlining obscures attribution. Uniform synthetic names can favor a table that real plugins do not use. Generated code can worsen rebuild fan-out and text size.
- Confidence: 94%
- Complexity: Medium

### 8. Delete the test-side builtin parser clone

- Description: Let production generation enforce completeness, uniqueness, sort order, arity, and method invariants. Keep independent syntax sentinels and runtime behavior tests. Remove the broad mirror only after each claimed defect class has a failing mutation.
- Axis: Compatibility truth and tests
- Basis: Direct: `crates/ox-eval/build.rs`; `crates/ox-eval/src/builtins_tests.rs`; generated builtin metadata consumed by `crates/ox-eval/src/builtins.rs`.
- Rationale: This deletes duplicate parser machinery without deleting independent defect detection.
- Downsides: Reduced parser diversity can miss a stable-count corruption, such as dropping one builtin while duplicating another. Exact names and uniqueness must remain independently checked.
- Confidence: 88%
- Complexity: Medium

### 9. Replace production test seams with observable behavior

- Description: Remove the four known test-only production seams only when a public editor or script scenario fails against the same seeded bug. Keep narrow private unit access where no observable path distinguishes the defect.
- Axis: Compatibility truth and tests
- Basis: Direct: test seams for `equalprg`, `resolve_method`, `has_pending_ctrl_bslash`, and `test_lvalue_parts` in `crates/ox-editor/src/indent.rs`, `crates/ox-editor/src/mode.rs`, and `crates/ox-editor/src/excmd_exec.rs`.
- Rationale: Tests protect observable behavior, and production loses representation-coupled test accessors.
- Downsides: End-to-end replacements can be vacuous, weaker, or sensitive to global state. Every replacement needs one-for-one mutation proof before deletion.
- Confidence: 91%
- Complexity: Medium

## Rejection summary

| Candidate | Disposition | Reason |
|---|---|---|
| Composition-owned `ApiSession` | Merged into idea 1 | It finds the lifetime root, but owning `Editor` inside an API facade overcouples the facade. |
| Borrowed `ApiCall` context | Merged into idea 1 | A lexical view is useful only with a durable runtime owner and host-availability rule. |
| Lexical request with dynamic host pool | Merged into idea 1 | The pool is the general depth rule; request ownership of every runtime field is not yet proved. |
| Server runtime bridge | Merged into idea 1 | Shared composition belongs with the runtime owner; error translation remains a separate compatibility decision. |
| Delete `RuntimeState` into natural owners | Rejected | Flag-day field diaspora has no atomic ownership ledger and risks illegal dependency moves. |
| One semantic registry path and one UI runtime | Deferred | It needs cross-origin redraw, channel-substitution, rollback, and indexed-error traces. |
| Process-wide reentrant Ex engine | Rejected | It conflicts with session isolation and depends on an unproved field reclassification. |
| Ex execution kernel move | Rejected | Moving a large file does not remove an owner or a truth. |
| Canonical command identity | Deferred | Exact spelling and behaviorally distinct aliases need an exhaustive corpus first. |
| Shared shell/process engine | Deferred | Sync and async launch paths need a differential trace for stderr, timing, events, and cancellation. |
| Quickfix and tag owners | Rejected | It bundles distinct transactional subsystems and mainly relocates code. |
| Single builtin capability catalog | Deferred into idea 7 | It needs dispatch attribution and a one-source prototype before production code. |
| Reentrant pumping behind a new safe loop owner | Deferred | Use only if the narrow idea 2 fails Miri. |
| Binding-owned callback failure channel | Deferred | No compatible delivery time or surfacing path is grounded. |
| Library-backed native grammar capability | Rejected | The current `Arc` already owns lifetime, and a wrapper cannot validate arbitrary DSO signatures. |
| Non-ABA Lua reference leases | Deferred | Reference reuse is proved; a reachable stale-owner failure is not. |
| Process-root global mutation owner | Deferred | It needs a complete actor and access map across LuaJIT, native plugins, libc, and dependency threads. |
| Scoped chunk-aware text view | Deferred into idea 6 | It earns code only if measured-hot consumers retain segmentation through their sinks. |
| Compile regex NFA once | Rejected | Lowering cost, failure timing, clone size, and AST retention are unmeasured. |
| Transfer rendered grid into history | Rejected | Clone cost and multi-channel failure-atomic ownership are unmeasured. |
| Resolve scope binding once | Rejected | No profile exists, and callback-crossing borrows can cost more than short scans. |
| Derive registration from `#[api]` | Rejected | Cross-module aggregation needs a scanner or giant macro that can miss cfg and syntax cases. |
| Module-owned builtin membership | Deferred into idea 7 | Local tables can duplicate dispatch truth and change lookup cost. |
| Dissolve task-named test suites | Rejected | Moving tests by taxonomy does not change behavior or ownership. |
| Consolidate local fixtures globally | Rejected | Same-named fakes do not share one stable contract. |
| Delete the quickfix placeholder | Rejected from architecture | It is safe local hygiene after a reference proof, but one empty file is not an architecture result. |
| Move runtime fields into `Editor` | Rejected | It preserves split ownership by turning the editor into a state container for unrelated API, UI, and transport concerns. |
| Scoped borrowed TLS or a global ID map | Rejected | Both retain ambient recovery and hidden lifetime rules. |
| `dyn RuntimeHost` and per-command trait objects | Rejected | They add indirection without protecting a second shipped adapter or measured hot path. |
| Size- or alphabet-based file splits | Rejected | File length is not an ownership decision. |
| New execution, quickfix, builtin, or test-support crates | Rejected | No independent dependency, target, unsafe seam, or stable shared contract earns them. |
| Queue or mutex based nested execution | Rejected | It changes synchronous reentry and adds contention to a single-threaded editor. |
| Blanket callback `catch_unwind` | Rejected | Containment without state restoration preserves corruption. |
| Leaked Tree-sitter libraries or runtime signature checks | Rejected | Leaks are not ownership, and portable loaders cannot validate C signatures. |
| `Arc<Mutex<UvLoop>>` | Rejected | It adds locking without solving callback aliasing or single-thread semantics. |
| Universal grid small strings, SIMD, or lock-free caches | Rejected | No measured allocation, compute, or contention bottleneck exists. |
| Universal compatibility snapshot | Rejected | It can self-grade against the same flawed decoder and duplicate canonical metadata. |
| Delete deprecated or unavailable APIs | Rejected | They remain part of Neovim compatibility. |

## Next decision

Choose whether the first architectural ticket starts after the current-directory repair, as the evidence requires, or whether the dirty directory change should be removed before the runtime rewrite begins. ~~No performance code is eligible before the stage-attribution work.~~ **Update 2026-09-03**: Stage attribution is complete (B1 baseline: `post-refactor-perf-baseline.md`, run `fe4f01dabb853150`). Rope (#19) and dispatch (#20) experiments returned REJECT with measured evidence (`accepted-perf-tree.md`). The `refresh_local_options` churn cut landed (`refresh-churn-cut.md`). The session pools from idea 1 have landed (`session.rs`).