//! Ordered decoration-provider registry and per-redraw aggregation.
//!
//! This module models the Neovim decoration provider lifecycle without
//! executing host callbacks. The phase contract is taken from:
//!
//! - `decoration_provider.c:108-125` — `decor_providers_start` ("start" phase).
//! - `decoration_provider.c:127-168` — `decor_providers_invoke_win` ("win" phase).
//! - `decoration_provider.c:170-196` — `decor_providers_invoke_line` ("line" phase).
//! - `decoration_provider.c:250-266` — `decor_providers_invoke_buf` ("buf" phase).
//! - `decoration_provider.c:268-284` — `decor_providers_invoke_end` ("end" phase).
//! - `decoration.c:737-751` — promotion of future `DecorRange`s into the active
//!   list sorted by `priority_internal` then `ordering`.
//! - `decoration.c:567-570` — `ordering` is the per-`DecorState` insertion order
//!   assigned by `decor_range_insert`.
//! - `decoration.h:34-61` — `DecorRange` fields, including `priority_internal`,
//!   `ordering`, `owned` (ephemeral) and `kind`.
//! - `decoration_defs.h:43-45` — `DecorPriority` / `DecorPriorityInternal` types.
//! - `decoration_defs.h:145-171` — `DecorProvider` callback refs and per-redraw state.

use std::collections::HashMap;
use std::marker::PhantomData;

use thiserror::Error;

/// Errors that can occur in the decoration subsystem.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DecorationError {
    /// A provider identifier is not registered.
    #[error("unknown decoration provider")]
    UnknownProvider,
    /// No further redraw identifier can be represented.
    #[error("redraw identifier space exhausted")]
    RedrawIdExhausted,
    /// An operation required a redraw identifier but none is active.
    #[error("no redraw is currently active")]
    NoActiveRedraw,
    /// A redraw identifier does not match the active redraw.
    #[error("redraw identifier does not match the active redraw")]
    InvalidRedrawId,
}

/// Zero-based, byte-oriented position inside a buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DecorPos {
    /// Zero-based buffer row.
    pub row: u32,
    /// Zero-based byte column.
    pub col: u32,
}

/// Inclusive range between two [`DecorPos`] values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DecorRange {
    /// Inclusive range start.
    pub start: DecorPos,
    /// Inclusive range end.
    pub end: DecorPos,
}

impl DecorRange {
    /// Returns `true` if `row` lies inside the inclusive row span of this range.
    #[must_use]
    pub fn contains_row(&self, row: u32) -> bool {
        self.start.row <= row && row <= self.end.row
    }

    /// Returns `true` if `pos` lies inside this inclusive range.
    #[must_use]
    pub fn contains(&self, pos: DecorPos) -> bool {
        if pos.row < self.start.row || pos.row > self.end.row {
            return false;
        }
        if pos.row == self.start.row && pos.col < self.start.col {
            return false;
        }
        if pos.row == self.end.row && pos.col > self.end.col {
            return false;
        }
        true
    }
}

/// Opaque provider identifier.
///
/// One namespace owns at most one provider, so the identifier is derived
/// from the namespace, matching upstream `get_decor_provider(ns, create)`
/// keyed identity (`decoration_provider.c:53-64`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(pub u32);

impl ProviderId {
    /// Creates the provider identity owned by `namespace`.
    #[must_use]
    pub const fn from_namespace(namespace: crate::extmark::NamespaceId) -> Self {
        Self(namespace.get())
    }

    /// Creates a provider identifier from a raw value.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}

/// Identifier of one active redraw transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RedrawId(pub u64);

/// Opaque window identifier used to scope ephemeral decorations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

/// Opaque buffer identifier used for `buf` phase plans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BufferId(pub u32);

/// Marker type for the `start` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StartPhase;

/// Marker type for the `buf` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufPhase;

/// Marker type for the `win` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WinPhase;

/// Marker type for the `line` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinePhase;

/// Marker type for the `range` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RangePhase;

/// Marker type for the `end` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EndPhase;

/// Marker type for the `_on_hl_def` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HlDefPhase;

/// Marker type for the `_on_spell_nav` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SpellNavPhase;

/// Marker type for the `_on_conceal_line` provider callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConcealLinePhase;

/// A typed callback handle for one of the decoration-provider phases.
///
/// The type parameter makes `on_buf`, `on_win` and `on_line` (and the other
/// lifecycle hooks) distinguishable at the type level, mirroring the separate
/// `LuaRef` fields of `DecorProvider` in `decoration_defs.h:145-171`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CallbackId<T> {
    id: u64,
    _phase: PhantomData<T>,
}

impl<T> CallbackId<T> {
    /// Construct a typed callback handle from its raw identifier.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self {
            id,
            _phase: PhantomData,
        }
    }

    /// Return the raw callback identifier.
    #[must_use]
    pub const fn get(&self) -> u64 {
        self.id
    }
}

/// Typed callback handle for the provider `start` phase.
pub type StartCallbackId = CallbackId<StartPhase>;

/// Typed callback handle for the provider `buf` phase.
pub type BufCallbackId = CallbackId<BufPhase>;

/// Typed callback handle for the provider `win` phase.
pub type WinCallbackId = CallbackId<WinPhase>;

/// Typed callback handle for the provider `line` phase.
pub type LineCallbackId = CallbackId<LinePhase>;

/// Typed callback handle for the provider `range` phase.
pub type RangeCallbackId = CallbackId<RangePhase>;

/// Typed callback handle for the provider `end` phase.
pub type EndCallbackId = CallbackId<EndPhase>;

/// Typed callback handle for the provider `_on_hl_def` hook.
pub type HlDefCallbackId = CallbackId<HlDefPhase>;

/// Typed callback handle for the provider `_on_spell_nav` hook.
pub type SpellNavCallbackId = CallbackId<SpellNavPhase>;

/// Typed callback handle for the provider `_on_conceal_line` hook.
pub type ConcealLineCallbackId = CallbackId<ConcealLinePhase>;

/// Lifecycle phase that produced an ephemeral decoration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallbackPhase {
    /// Whole-redraw start callback.
    Start,
    /// Buffer callback.
    Buf,
    /// Window callback.
    Win,
    /// Per-line callback.
    Line,
    /// Character-range callback.
    Range,
    /// Whole-redraw end callback.
    End,
}

/// Virtual text anchor position.
///
/// Mirrors `VirtTextPos` in `decoration_defs.h:19-27`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum VirtTextPos {
    /// End of the buffer line.
    #[default]
    Eol,
    /// Right-aligned after end-of-line text.
    EolRightAlign,
    /// Inserted inline with buffer text.
    Inline,
    /// Drawn over buffer text.
    Overlay,
    /// Right-aligned in the window.
    RightAlign,
    /// At an explicit window column.
    WinCol,
}

/// A single chunk of virtual text with an optional highlight group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct VirtTextChunk {
    /// Displayed text.
    pub text: String,
    /// Optional highlight group.
    pub hl_group: Option<String>,
}

/// Virtual text rendered at a single anchor point.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct VirtualText {
    /// Anchor mode.
    pub pos: VirtTextPos,
    /// Explicit window column when applicable.
    pub col: Option<u32>,
    /// Ordered virtual-text chunks.
    pub chunks: Vec<VirtTextChunk>,
}

/// Virtual lines rendered above or below a buffer row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct VirtualLines {
    /// Ordered virtual lines and their chunks.
    pub lines: Vec<Vec<VirtTextChunk>>,
    /// Rendering flags retained for the UI layer.
    pub flags: u32,
}

/// Origin of an aggregated decoration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecorOrigin {
    /// Persistent decoration from an extmark. `order` is a stable, per-extmark
    /// insertion order used for deterministic tie-breaking.
    Extmark {
        /// Namespace owning the extmark.
        namespace: u32,
        /// Namespace-local extmark identifier.
        mark_id: u32,
        /// Stable insertion order.
        order: u64,
    },
    /// Ephemeral decoration produced by a provider callback during a redraw.
    Provider {
        /// Provider that emitted the decoration.
        provider: ProviderId,
        /// Callback phase that emitted it.
        phase: CallbackPhase,
    },
}

/// An aggregated decoration record.
///
/// Records are ordered by `priority` (lower first, following the active
/// `DecorRange` sort in `decoration.c:737-751`), then by origin (`extmark`
/// before `provider`) and finally by a stable insertion/registration order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DecorItem {
    /// Where this decoration came from (extmark or a provider phase).
    pub origin: DecorOrigin,
    /// Window scope, if this decoration was produced for a specific window.
    pub window: Option<WindowId>,
    /// Inclusive buffer range this decoration applies to.
    pub range: DecorRange,
    /// Display priority, matching `DecorPriority` in `decoration_defs.h:43-45`.
    pub priority: u32,
    /// `winblend` value for floating windows, if any.
    pub winblend: Option<u16>,
    /// Virtual text chunks, if any.
    pub virt_text: Option<VirtualText>,
    /// Virtual lines, if any.
    pub virt_lines: Option<VirtualLines>,
}

/// Domain input for building an ephemeral provider decoration.
///
/// Bundles the fields [`DecorItem::for_provider`] needs so the constructor
/// takes a single cohesive input rather than a long positional list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderDecorInput {
    /// Owning provider identifier.
    pub provider: ProviderId,
    /// Lifecycle phase that produced the decoration.
    pub phase: CallbackPhase,
    /// Window scope for the decoration.
    pub window: WindowId,
    /// Inclusive buffer range this decoration applies to.
    pub range: DecorRange,
    /// Display priority, matching `DecorPriority` in `decoration_defs.h:43-45`.
    pub priority: u32,
    /// `winblend` value for floating windows, if any.
    pub winblend: Option<u16>,
    /// Virtual text chunks, if any.
    pub virt_text: Option<VirtualText>,
    /// Virtual lines, if any.
    pub virt_lines: Option<VirtualLines>,
}

impl Default for ProviderDecorInput {
    fn default() -> Self {
        Self {
            provider: ProviderId::default(),
            phase: CallbackPhase::Start,
            window: WindowId::default(),
            range: DecorRange::default(),
            priority: 0,
            winblend: None,
            virt_text: None,
            virt_lines: None,
        }
    }
}

impl DecorItem {
    /// Convenience constructor for an ephemeral provider decoration.
    ///
    /// The decoration's origin is [`DecorOrigin::Provider`] with `input`'s
    /// provider and phase; `window` is wrapped in `Some` to scope the
    /// decoration to that window.
    #[must_use]
    pub fn for_provider(input: ProviderDecorInput) -> Self {
        let ProviderDecorInput {
            provider,
            phase,
            window,
            range,
            priority,
            winblend,
            virt_text,
            virt_lines,
        } = input;
        Self {
            origin: DecorOrigin::Provider { provider, phase },
            window: Some(window),
            range,
            priority,
            winblend,
            virt_text,
            virt_lines,
        }
    }
}

/// Callback handles supplied when registering a decoration provider.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DecorProviderDef {
    /// Optional redraw-start callback.
    pub start: Option<StartCallbackId>,
    /// Optional buffer callback.
    pub buf: Option<BufCallbackId>,
    /// Optional window callback.
    pub win: Option<WinCallbackId>,
    /// Optional line callback.
    pub line: Option<LineCallbackId>,
    /// Optional range callback.
    pub range: Option<RangeCallbackId>,
    /// Optional redraw-end callback.
    pub end: Option<EndCallbackId>,
    /// Optional `_on_hl_def` highlight-definition callback.
    ///
    /// Stored for parity with upstream `DecorProvider.hl_def`
    /// (`decoration_defs.h:164`); no dispatch site exists yet — invocation
    /// lands with the corresponding redraw event.
    pub hl_def: Option<HlDefCallbackId>,
    /// Optional `_on_spell_nav` spell-navigation callback
    /// (`decoration_defs.h:165`); stored, not yet invoked.
    pub spell_nav: Option<SpellNavCallbackId>,
    /// Optional `_on_conceal_line` callback (`decoration_defs.h:166`);
    /// stored, not yet invoked.
    pub conceal_line: Option<ConcealLineCallbackId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RegisteredProvider {
    id: ProviderId,
    order: usize,
    enabled: bool,
    callbacks: DecorProviderDef,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredEphemeral {
    item: DecorItem,
    seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RedrawState {
    id: RedrawId,
    display_tick: u64,
    ephemeral: Vec<StoredEphemeral>,
    next_seq: u64,
}

/// Ordered decoration-provider registry.
///
/// Providers are keyed by their owning namespace, so at most one provider
/// exists per namespace (`decoration_provider.c:53-64`). First installation
/// appends to registration order; replacement and clearing preserve it. The
/// registry tracks one active redraw at a time through [`RedrawId`] values so
/// callback-triggered nested redraws never borrow the registry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decorations {
    providers: HashMap<ProviderId, RegisteredProvider>,
    order: Vec<ProviderId>,
    next_redraw: u64,
    redraw_state: Option<RedrawState>,
}

/// Result of beginning a redraw cycle.
///
/// `Outermost` owns the new transaction and must be finished with
/// [`Decorations::finish_redraw`]. `Nested` means a redraw is already active:
/// the caller performs only ordinary non-provider work and runs no provider
/// callbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedrawEntry {
    /// The only active redraw transaction.
    Outermost(RedrawId),
    /// A redraw is already active; nested entry runs no callbacks.
    Nested,
}

impl Decorations {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install or replace the provider owned by `id`.
    ///
    /// First installation appends `id` to registration order; replacement
    /// keeps the existing position. Returns the previous callback definition
    /// so its Lua references can be released exactly once.
    ///
    /// # Errors
    ///
    /// Currently infallible; the [`Result`] is retained for API consistency
    /// with the rest of the provider lifecycle.
    pub fn replace_provider(
        &mut self,
        id: ProviderId,
        def: DecorProviderDef,
    ) -> Result<Option<DecorProviderDef>, DecorationError> {
        if let Some(provider) = self.providers.get_mut(&id) {
            let previous = std::mem::replace(&mut provider.callbacks, def);
            return Ok(Some(previous));
        }
        let order = self.order.len();
        let provider = RegisteredProvider {
            id,
            order,
            enabled: true,
            callbacks: def,
        };
        self.providers.insert(id, provider);
        self.order.push(id);
        Ok(None)
    }

    /// Clear the provider owned by `id`, keeping its registration slot.
    ///
    /// Returns the removed callback definition for release; `None` when no
    /// provider was installed. The stable registration slot is retained so a
    /// later reinstall keeps upstream's registration order.
    pub fn clear_provider(&mut self, id: ProviderId) -> Option<DecorProviderDef> {
        let provider = self.providers.get_mut(&id)?;
        Some(std::mem::take(&mut provider.callbacks))
    }

    /// Remove a provider from the registry entirely, returning its callback
    /// set if it existed.
    pub fn remove(&mut self, id: ProviderId) -> Option<DecorProviderDef> {
        let removed = self.providers.remove(&id)?;
        self.order.retain(|&registered_id| registered_id != id);
        Some(removed.callbacks)
    }

    /// Enable or disable a registered provider. Disabled providers are omitted
    /// from redraw phase plans.
    ///
    /// # Errors
    ///
    /// Returns [`DecorationError::UnknownProvider`] when `id` is not a
    /// registered provider.
    pub fn set_enabled(&mut self, id: ProviderId, enabled: bool) -> Result<(), DecorationError> {
        let provider = self
            .providers
            .get_mut(&id)
            .ok_or(DecorationError::UnknownProvider)?;
        provider.enabled = enabled;
        Ok(())
    }

    /// Return whether `id` is a registered provider with any callback.
    #[must_use]
    pub fn is_registered(&self, id: ProviderId) -> bool {
        self.providers
            .get(&id)
            .is_some_and(|provider| provider.enabled)
    }

    /// Return the registered provider IDs in stable registration order.
    ///
    /// This order is the tie-breaker for provider-originated ephemeral
    /// decorations, matching the `for` loops over `decor_providers` in
    /// `decoration_provider.c:108-284`.
    #[must_use]
    pub fn provider_order(&self) -> &[ProviderId] {
        &self.order
    }

    /// Return the registration index of `id`, if known.
    #[must_use]
    pub fn registration_index(&self, id: ProviderId) -> Option<usize> {
        self.providers.get(&id).map(|p| p.order)
    }

    /// Return the callback definition for a provider, if known.
    #[must_use]
    pub fn provider_def(&self, id: ProviderId) -> Option<&DecorProviderDef> {
        self.providers.get(&id).map(|p| &p.callbacks)
    }

    /// Begin a redraw cycle.
    ///
    /// A nested entry (a redraw is already active) performs no provider work
    /// and cannot end the outer transaction; callers run only the ordinary
    /// command work of the nested redraw.
    ///
    /// # Errors
    ///
    /// Returns [`DecorationError::RedrawIdExhausted`] when the redraw
    /// identifier space is exhausted (the internal counter would overflow).
    pub fn enter_redraw(&mut self, display_tick: u64) -> Result<RedrawEntry, DecorationError> {
        if self.redraw_state.is_some() {
            return Ok(RedrawEntry::Nested);
        }

        let new_next = self
            .next_redraw
            .checked_add(1)
            .ok_or(DecorationError::RedrawIdExhausted)?;
        let id = RedrawId(self.next_redraw);
        self.next_redraw = new_next;

        self.redraw_state = Some(RedrawState {
            id,
            display_tick,
            ephemeral: Vec::new(),
            next_seq: 0,
        });
        Ok(RedrawEntry::Outermost(id))
    }

    /// End the redraw owned by `id`, discarding its ephemeral state.
    ///
    /// # Errors
    ///
    /// Returns [`DecorationError::NoActiveRedraw`] when no redraw is active,
    /// or [`DecorationError::InvalidRedrawId`] when `id` does not match the
    /// active redraw.
    pub fn finish_redraw(&mut self, id: RedrawId) -> Result<(), DecorationError> {
        let active = self
            .redraw_state
            .as_ref()
            .map(|state| state.id)
            .ok_or(DecorationError::NoActiveRedraw)?;
        if active != id {
            return Err(DecorationError::InvalidRedrawId);
        }
        self.redraw_state = None;
        Ok(())
    }

    /// Snapshot the provider IDs whose current definition carries `phase`.
    ///
    /// IDs are captured in registration order, but the callback itself is
    /// re-read through [`Decorations::phase_callback`] immediately before each
    /// invocation so a callback that replaces or removes another provider is
    /// observed by later invocations in the same frame.
    #[must_use]
    pub fn phase_provider_ids(&self, phase: CallbackPhase) -> Vec<ProviderId> {
        self.order
            .iter()
            .copied()
            .filter(|&id| {
                self.providers.get(&id).is_some_and(|provider| {
                    provider.enabled && phase_callback(provider, phase).is_some()
                })
            })
            .collect()
    }

    /// Re-read the current callback reference for `phase` on `id`.
    ///
    /// Returns `None` when the provider was removed, replaced without this
    /// phase, or disabled since the snapshot, matching upstream's
    /// per-invocation `decor_provider_invoke` lookup.
    #[must_use]
    pub fn phase_callback(&self, id: ProviderId, phase: CallbackPhase) -> Option<u64> {
        let provider = self.providers.get(&id)?;
        if !provider.enabled {
            return None;
        }
        phase_callback(provider, phase)
    }

    /// Append an ephemeral decoration for the active redraw.
    ///
    /// # Errors
    ///
    /// Returns [`DecorationError::NoActiveRedraw`] when no redraw is active,
    /// [`DecorationError::InvalidRedrawId`] when `id` does not match the
    /// active redraw, [`DecorationError::UnknownProvider`] when the item's
    /// origin references an unregistered provider, or
    /// [`DecorationError::RedrawIdExhausted`] when the ephemeral sequence
    /// counter would overflow.
    pub fn push_ephemeral(&mut self, id: RedrawId, item: DecorItem) -> Result<(), DecorationError> {
        let state = self
            .redraw_state
            .as_mut()
            .ok_or(DecorationError::NoActiveRedraw)?;
        if state.id != id {
            return Err(DecorationError::InvalidRedrawId);
        }
        if let DecorOrigin::Provider { provider, .. } = &item.origin
            && !self.providers.contains_key(provider)
        {
            return Err(DecorationError::UnknownProvider);
        }
        let seq = state.next_seq;
        state.next_seq = state
            .next_seq
            .checked_add(1)
            .ok_or(DecorationError::RedrawIdExhausted)?;
        state.ephemeral.push(StoredEphemeral { item, seq });
        Ok(())
    }

    /// The display tick the active redraw was started with.
    #[must_use]
    pub fn active_display_tick(&self, id: RedrawId) -> Option<u64> {
        self.redraw_state
            .as_ref()
            .filter(|state| state.id == id)
            .map(|state| state.display_tick)
    }

    /// Query decorations for a specific window row.
    ///
    /// `persistent` are the extmark-derived decorations for the buffer (or
    /// any other persistent source). They are combined with the ephemeral
    /// decorations collected during the active redraw identified by `id`,
    /// filtered to `window` and `row`, and returned in deterministic order.
    ///
    /// The ordering follows the `DecorState` active range contract:
    /// primary `priority`, then extmark origin before provider origin, then
    /// stable insertion/registration order (`decoration.c:737-751`).
    #[must_use]
    pub fn query_line(
        &self,
        id: RedrawId,
        persistent: &[DecorItem],
        window: WindowId,
        row: u32,
    ) -> Vec<DecorItem> {
        let decorations = &self;
        let Some(state) = decorations.redraw_state.as_ref() else {
            return Vec::new();
        };
        if state.id != id {
            return Vec::new();
        }

        let mut scored: Vec<(DecorItem, SortKey)> = Vec::new();

        for item in persistent {
            if !Self::item_matches(item, window, row) {
                continue;
            }
            let key = decorations.sort_key(item, None);
            scored.push((item.clone(), key));
        }

        for stored in &state.ephemeral {
            let item = &stored.item;
            if !Self::item_matches(item, window, row) {
                continue;
            }
            let key = decorations.sort_key(item, Some(stored.seq));
            scored.push((item.clone(), key));
        }

        scored.sort_by_key(|a| a.1);
        scored.into_iter().map(|(item, _)| item).collect()
    }

    fn sort_key(&self, item: &DecorItem, seq: Option<u64>) -> SortKey {
        let (category, reg_index, suborder) = match &item.origin {
            DecorOrigin::Extmark { order, .. } => (0, 0, *order),
            DecorOrigin::Provider { provider, .. } => {
                let idx = self.providers.get(provider).map_or(usize::MAX, |p| p.order);
                let reg = idx as u64;
                let sub = seq.unwrap_or(0);
                (1, reg, sub)
            }
        };

        SortKey {
            priority: item.priority,
            category,
            reg_index,
            suborder,
        }
    }

    fn item_matches(item: &DecorItem, window: WindowId, row: u32) -> bool {
        if let Some(w) = item.window
            && w != window
        {
            return false;
        }
        item.range.contains_row(row)
    }
}

fn phase_callback(provider: &RegisteredProvider, phase: CallbackPhase) -> Option<u64> {
    let callbacks = &provider.callbacks;
    match phase {
        CallbackPhase::Start => callbacks.start.map(|cb| cb.get()),
        CallbackPhase::Buf => callbacks.buf.map(|cb| cb.get()),
        CallbackPhase::Win => callbacks.win.map(|cb| cb.get()),
        CallbackPhase::Line => callbacks.line.map(|cb| cb.get()),
        CallbackPhase::Range => callbacks.range.map(|cb| cb.get()),
        CallbackPhase::End => callbacks.end.map(|cb| cb.get()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey {
    priority: u32,
    category: u8,
    reg_index: u64,
    suborder: u64,
}
