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
    /// A second redraw was started before the active redraw ended.
    #[error("a redraw is already in progress")]
    AlreadyRedrawing,
    /// An operation required a redraw token but no redraw was active.
    #[error("no redraw is currently active")]
    NoActiveRedraw,
    /// A provider identifier is not registered.
    #[error("unknown decoration provider")]
    UnknownProvider,
    /// No further provider identifier can be represented.
    #[error("provider identifier space exhausted")]
    ProviderIdExhausted,
    /// No further redraw identifier can be represented.
    #[error("redraw identifier space exhausted")]
    RedrawIdExhausted,
    /// A redraw token does not own the currently active redraw.
    #[error("redraw token does not match the active redraw")]
    InvalidRedrawToken,
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
    pub fn contains_row(&self, row: u32) -> bool {
        self.start.row <= row && row <= self.end.row
    }

    /// Returns `true` if `pos` lies inside this inclusive range.
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

/// Opaque provider identifier, assigned at registration time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(pub u32);

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
    pub const fn new(id: u64) -> Self {
        Self {
            id,
            _phase: PhantomData,
        }
    }

    /// Return the raw callback identifier.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VirtTextPos {
    /// End of the buffer line.
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

impl Default for VirtTextPos {
    fn default() -> Self {
        VirtTextPos::Eol
    }
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

impl DecorItem {
    /// Convenience constructor for an ephemeral provider decoration.
    pub fn for_provider(
        provider: ProviderId,
        phase: CallbackPhase,
        window: WindowId,
        range: DecorRange,
        priority: u32,
        winblend: Option<u16>,
        virt_text: Option<VirtualText>,
        virt_lines: Option<VirtualLines>,
    ) -> Self {
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

/// Per-phase invocation plan for a redraw cycle.
///
/// Providers appear in the same order they were registered, matching the
/// iteration order in `decoration_provider.c:108-284`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhasePlan {
    /// Start callbacks in registration order.
    pub start: Vec<(ProviderId, StartCallbackId)>,
    /// Buffer callbacks in registration order.
    pub buf: Vec<(ProviderId, BufCallbackId)>,
    /// Window callbacks in registration order.
    pub win: Vec<(ProviderId, WinCallbackId)>,
    /// Line callbacks in registration order.
    pub line: Vec<(ProviderId, LineCallbackId)>,
    /// Range callbacks in registration order.
    pub range: Vec<(ProviderId, RangeCallbackId)>,
    /// End callbacks in registration order.
    pub end: Vec<(ProviderId, EndCallbackId)>,
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
    id: u64,
    display_tick: u64,
    ephemeral: Vec<StoredEphemeral>,
    next_seq: u64,
}

/// Ordered decoration-provider registry.
///
/// Providers are stored in registration order. The registry can begin a single
/// redraw at a time; the returned [`DecorRedrawToken`] owns the ephemeral
/// decoration store and is the only way to add or query ephemeral output for
/// that redraw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decorations {
    providers: HashMap<ProviderId, RegisteredProvider>,
    order: Vec<ProviderId>,
    next_id: u32,
    next_redraw: u64,
    redraw_state: Option<RedrawState>,
}

impl Decorations {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new decoration provider and return its stable [`ProviderId`].
    pub fn register(&mut self, def: DecorProviderDef) -> Result<ProviderId, DecorationError> {
        let new_next = self
            .next_id
            .checked_add(1)
            .ok_or(DecorationError::ProviderIdExhausted)?;
        let id = ProviderId(self.next_id);
        self.next_id = new_next;

        let order = self.order.len();
        let provider = RegisteredProvider {
            id,
            order,
            enabled: true,
            callbacks: def,
        };
        self.providers.insert(id, provider);
        self.order.push(id);
        Ok(id)
    }

    /// Replace the callback set for an existing provider.
    pub fn update(
        &mut self,
        id: ProviderId,
        def: DecorProviderDef,
    ) -> Result<(), DecorationError> {
        let provider = self
            .providers
            .get_mut(&id)
            .ok_or(DecorationError::UnknownProvider)?;
        provider.callbacks = def;
        Ok(())
    }

    /// Remove a provider from the registry, returning its callback set if it
    /// existed.
    pub fn remove(&mut self, id: ProviderId) -> Option<DecorProviderDef> {
        let removed = self.providers.remove(&id)?;
        self.order.retain(|&registered_id| registered_id != id);
        Some(removed.callbacks)
    }

    /// Enable or disable a registered provider. Disabled providers are omitted
    /// from redraw phase plans.
    pub fn set_enabled(&mut self, id: ProviderId, enabled: bool) -> Result<(), DecorationError> {
        let provider = self
            .providers
            .get_mut(&id)
            .ok_or(DecorationError::UnknownProvider)?;
        provider.enabled = enabled;
        Ok(())
    }

    /// Return whether `id` is a registered provider.
    pub fn is_registered(&self, id: ProviderId) -> bool {
        self.providers.contains_key(&id)
    }

    /// Return the registered provider IDs in stable registration order.
    ///
    /// This order is the tie-breaker for provider-originated ephemeral
    /// decorations, matching the `for` loops over `decor_providers` in
    /// `decoration_provider.c:108-284`.
    pub fn provider_order(&self) -> &[ProviderId] {
        &self.order
    }

    /// Return the registration index of `id`, if known.
    pub fn registration_index(&self, id: ProviderId) -> Option<usize> {
        self.providers.get(&id).map(|p| p.order)
    }

    /// Return the callback definition for a provider, if known.
    pub fn provider_def(&self, id: ProviderId) -> Option<&DecorProviderDef> {
        self.providers.get(&id).map(|p| &p.callbacks)
    }

    /// Begin a new redraw cycle.
    ///
    /// Only one redraw can be active at a time. The returned token must be
    /// ended (or dropped) before another redraw can begin.
    pub fn begin_redraw(
        &mut self,
        display_tick: u64,
    ) -> Result<DecorRedrawToken<'_>, DecorationError> {
        if self.redraw_state.is_some() {
            return Err(DecorationError::AlreadyRedrawing);
        }

        let new_next = self
            .next_redraw
            .checked_add(1)
            .ok_or(DecorationError::RedrawIdExhausted)?;
        let id = self.next_redraw;
        self.next_redraw = new_next;

        self.redraw_state = Some(RedrawState {
            id,
            display_tick,
            ephemeral: Vec::new(),
            next_seq: 0,
        });

        Ok(DecorRedrawToken {
            decorations: self,
            id,
            ended: false,
        })
    }

    fn finish_redraw(&mut self, id: u64) -> Result<(), DecorationError> {
        let active_id = match self.redraw_state.as_ref() {
            Some(state) => state.id,
            None => return Err(DecorationError::NoActiveRedraw),
        };

        if active_id != id {
            return Err(DecorationError::InvalidRedrawToken);
        }

        self.redraw_state = None;
        Ok(())
    }

    fn sort_key(&self, item: &DecorItem, seq: Option<u64>) -> SortKey {
        let (category, reg_index, suborder) = match &item.origin {
            DecorOrigin::Extmark { order, .. } => (0, 0, *order),
            DecorOrigin::Provider { provider, .. } => {
                let idx = self
                    .providers
                    .get(provider)
                    .map(|p| p.order)
                    .unwrap_or(usize::MAX);
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
}

/// Redraw lifecycle token.
///
/// Holds the mutable borrow of [`Decorations`] for the duration of one redraw.
/// Ephemeral decorations added through this token are dropped when the token
/// is ended or dropped, making them inaccessible afterwards.
#[derive(Debug)]
pub struct DecorRedrawToken<'a> {
    decorations: &'a mut Decorations,
    id: u64,
    ended: bool,
}

impl<'a> DecorRedrawToken<'a> {
    /// Return the display tick this redraw was started with.
    pub fn display_tick(&self) -> u64 {
        let decorations = &*self.decorations;
        decorations
            .redraw_state
            .as_ref()
            .map_or(0, |state| state.display_tick)
    }

    /// Return the active provider IDs in registration order.
    pub fn provider_order(&self) -> &[ProviderId] {
        let decorations = &*self.decorations;
        &decorations.order
    }

    /// Append an ephemeral decoration for the current redraw.
    pub fn push_ephemeral(&mut self, item: DecorItem) -> Result<(), DecorationError> {
        let decorations = &mut *self.decorations;
        let state = decorations
            .redraw_state
            .as_mut()
            .ok_or(DecorationError::NoActiveRedraw)?;

        if state.id != self.id {
            return Err(DecorationError::InvalidRedrawToken);
        }

        if let DecorOrigin::Provider { provider, .. } = &item.origin {
            if !decorations.providers.contains_key(provider) {
                return Err(DecorationError::UnknownProvider);
            }
        }

        let seq = state.next_seq;
        state.next_seq = state
            .next_seq
            .checked_add(1)
            .ok_or(DecorationError::RedrawIdExhausted)?;

        state.ephemeral.push(StoredEphemeral { item, seq });
        Ok(())
    }

    /// Return the invocation plan for each lifecycle phase.
    ///
    /// Providers are listed in registration order, with only the callbacks that
    /// were supplied at registration time included.
    pub fn phase_plans(&self) -> PhasePlan {
        let decorations = &*self.decorations;
        let mut plan = PhasePlan::default();

        for &id in &decorations.order {
            if let Some(provider) = decorations.providers.get(&id) {
                if !provider.enabled {
                    continue;
                }
                if let Some(cb) = provider.callbacks.start {
                    plan.start.push((id, cb));
                }
                if let Some(cb) = provider.callbacks.buf {
                    plan.buf.push((id, cb));
                }
                if let Some(cb) = provider.callbacks.win {
                    plan.win.push((id, cb));
                }
                if let Some(cb) = provider.callbacks.line {
                    plan.line.push((id, cb));
                }
                if let Some(cb) = provider.callbacks.range {
                    plan.range.push((id, cb));
                }
                if let Some(cb) = provider.callbacks.end {
                    plan.end.push((id, cb));
                }
            }
        }

        plan
    }

    /// Query decorations for a specific window row.
    ///
    /// `persistent` are the extmark-derived decorations for the buffer (or
    /// any other persistent source). They are combined with the ephemeral
    /// decorations collected during this redraw, filtered to `window` and
    /// `row`, and returned in deterministic order.
    ///
    /// The ordering follows the `DecorState` active range contract:
    /// primary `priority`, then extmark origin before provider origin, then
    /// stable insertion/registration order (`decoration.c:737-751`).
    pub fn query_line(
        &self,
        persistent: &[DecorItem],
        window: WindowId,
        row: u32,
    ) -> Vec<DecorItem> {
        let decorations = &*self.decorations;
        let Some(state) = decorations.redraw_state.as_ref() else {
            return Vec::new();
        };

        if state.id != self.id {
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

        scored.sort_by(|a, b| a.1.cmp(&b.1));
        scored.into_iter().map(|(item, _)| item).collect()
    }

    /// End the current redraw and discard all ephemeral decorations.
    pub fn end(mut self) -> Result<(), DecorationError> {
        self.decorations.finish_redraw(self.id)?;
        self.ended = true;
        Ok(())
    }

    fn item_matches(item: &DecorItem, window: WindowId, row: u32) -> bool {
        if let Some(w) = item.window {
            if w != window {
                return false;
            }
        }
        item.range.contains_row(row)
    }
}

impl<'a> Drop for DecorRedrawToken<'a> {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.decorations.finish_redraw(self.id);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey {
    priority: u32,
    category: u8,
    reg_index: u64,
    suborder: u64,
}
