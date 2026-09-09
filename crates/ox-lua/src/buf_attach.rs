//! Delivery of `nvim_buf_attach` Lua callbacks and RPC events for committed
//! buffer updates.
//!
//! The editor records one [`BufferBytesEvent`] per committed buffer update in
//! the buffer (see `BufferState::take_bytes_events`); the callbacks themselves
//! are Lua registry references owned by this layer, so the drain lives here.
//! Callers drain at every transition into user Lua (chunk entries, callback
//! invocations, and after each `vim.api` dispatch): the collect phase runs
//! under the editor borrow and the invoke phase runs without it, so user code
//! can reenter while the queues stay consistent. A failing listener does not
//! starve the rest: the first error returns after the drain completes, and the
//! edit it observed already stands.
//!
//! `on_changedtick` rides the same committed-update queue as `on_lines` and
//! `on_bytes`. `on_detach` fires from the release records, before their
//! registry references are freed, so a detaching plugin still sees the
//! callback. `on_reload` rides that same queue too: `BufferState::load`
//! enqueues a whole-buffer event for active attachments when a loaded buffer is
//! re-read (the `:edit!` path); the drain keeps attachments that supply
//! `on_reload`, detaches the rest, and ends RPC channels as upstream requires.

use std::collections::{BTreeMap, HashSet};

use mlua::{Lua, Value, Variadic};
use ox_api::{ApiSession, DispatchFn};
use ox_editor::{
    buffer::{BufferSubscriptionRelease, BufferUpdateKind},
    BufferAttachSubscription, BufferBytesEvent, BufferState,
};
use ox_rpc::Message;
use ox_types::{BufHandle, Object, OxStr};

use crate::converter::{free_lua_ref, object_to_lua};
use crate::vim::ApiDispatchContext;

/// One subscription's Lua registry reference plus the identity that owns it.
///
/// The id is needed so a truthy callback return can detach exactly the
/// subscription that produced it, even if user code attaches or detaches
/// other subscriptions while the drain is running.
struct CallbackRef {
    id: u128,
    reference: i32,
    utf_sizes: bool,
}

enum ReloadAction {
    Keep(CallbackRef),
    Detach(u128),
}

/// One buffer's drained callbacks and events. Built under the editor borrow;
/// invoked after it is released.
struct PendingDelivery {
    buffer: BufHandle,
    events: Vec<EventDelivery>,
}

struct PendingBatch {
    deliveries: Vec<PendingDelivery>,
    released: Vec<BufferSubscriptionRelease>,
}

struct EventDelivery {
    event: BufferBytesEvent,
    reload_actions: Vec<ReloadAction>,
    reload_channels: Vec<(u128, u64)>,
    line_refs: Vec<CallbackRef>,
    byte_refs: Vec<CallbackRef>,
    tick_refs: Vec<CallbackRef>,
    /// `(subscription id, channel id)` pairs. Keeping the subscription id
    /// lets a failed transport retry only the recipient that did not receive
    /// this event, without duplicating successful sends.
    rpc_channels: Vec<(u128, u64)>,
}

/// Resolves one Lua callback reference for a single subscription, if any.
fn callback_ref(
    state: &BufferState,
    id: u128,
    key: &OxStr,
    utf_sizes_key: &OxStr,
) -> Option<CallbackRef> {
    let subscription = state.subscriptions().get(&id)?;
    let reference = match subscription.options.get(key)? {
        Object::LuaRef(reference) => *reference,
        _ => return None,
    };
    Some(CallbackRef {
        id,
        reference,
        utf_sizes: matches!(
            subscription.options.get(utf_sizes_key),
            Some(Object::Boolean(true))
        ),
    })
}

/// Releases every Lua registry reference nested in one object.
fn release_object_refs(lua: &Lua, object: &Object) -> Result<(), String> {
    let mut first_error = None;
    match object {
        Object::LuaRef(reference) => {
            if let Err(error) = free_lua_ref(lua, *reference).map_err(|error| error.to_string()) {
                first_error = Some(error);
            }
        }
        Object::Array(values) => {
            for value in values {
                if let Err(error) = release_object_refs(lua, value)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }
        Object::Dict(dict) => {
            for (_, value) in dict.iter() {
                if let Err(error) = release_object_refs(lua, value)
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
        }
        _ => {}
    }
    first_error.map_or(Ok(()), Err)
}

/// Returns the callback reference for one detach notification.
fn detach_callback_ref(subscription: &BufferAttachSubscription) -> Option<i32> {
    match subscription.options.get(&OxStr::from("on_detach")) {
        Some(Object::LuaRef(reference)) => Some(*reference),
        _ => None,
    }
}

/// Invokes `on_detach` before freeing the subscription's registry references.
///
/// The release records were collected while the editor was borrowed, but this
/// function receives owned records after that borrow has ended. This remains
/// true for wiped buffers, whose handles are carried by the release record.
fn invoke_detach_callbacks(
    lua: &Lua,
    context: &ApiDispatchContext,
    releases: &[BufferSubscriptionRelease],
) -> Result<(), String> {
    let mut first_error = None;
    for release in releases {
        let Some(reference) = detach_callback_ref(&release.subscription) else {
            continue;
        };
        let args = vec![
            Value::String(
                lua.create_string("detach")
                    .map_err(|error| error.to_string())?,
            ),
            Value::Integer(i64::from(release.buffer)),
        ];
        if let Err(error) = invoke_callback(lua, context, reference, args) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Releases every callback reference owned by one removed subscription.
fn release_subscription_refs(
    lua: &Lua,
    subscription: &BufferAttachSubscription,
) -> Result<(), String> {
    let mut first_error = None;
    for (_, value) in subscription.options.iter() {
        if let Err(error) = release_object_refs(lua, value)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Releases callback references from subscriptions removed while the editor
/// was borrowed by a caller.
///
/// The caller must invoke this only after the editor borrow has ended; Lua
/// registry access is user-facing runtime work and must never overlap it.
pub fn release_removed_subscriptions(
    lua: &Lua,
    subscriptions: &[BufferSubscriptionRelease],
) -> Result<(), String> {
    let mut first_error = None;
    for release in subscriptions {
        if let Err(error) = release_subscription_refs(lua, &release.subscription)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Collects every pending event, callback reference, and removed subscription,
/// emptying all queues while the editor borrow is held.
fn collect_pending(session: &ApiSession) -> PendingBatch {
    let line_key = OxStr::from("on_lines");
    let bytes_key = OxStr::from("on_bytes");
    let tick_key = OxStr::from("on_changedtick");
    let reload_key = OxStr::from("on_reload");
    let utf_sizes_key = OxStr::from("utf_sizes");
    session.with_editor_mut(|editor| {
        let mut pending = PendingBatch {
            deliveries: Vec::new(),
            released: editor.take_pending_subscription_releases(),
        };
        for handle in editor.buffers() {
            let Ok(state) = editor.buffer_mut(handle) else {
                continue;
            };
            let events = state.take_bytes_events();
            let released = state.take_pending_subscription_releases_for(handle);
            pending.released.extend(released);
            if events.is_empty() {
                continue;
            }
            let mut deliveries = Vec::with_capacity(events.len());
            for event in events {
                let mut delivery = EventDelivery {
                    event,
                    reload_actions: Vec::new(),
                    reload_channels: Vec::new(),
                    line_refs: Vec::new(),
                    byte_refs: Vec::new(),
                    tick_refs: Vec::new(),
                    rpc_channels: Vec::new(),
                };
                for id in delivery.event.subscribers.iter().copied() {
                    let Some(subscription) = state.subscriptions().get(&id) else {
                        continue;
                    };
                    if matches!(&delivery.event.update, BufferUpdateKind::Reload) {
                        if subscription.channel_id != 0 {
                            delivery
                                .reload_channels
                                .push((id, subscription.channel_id));
                        } else if let Some(callback) =
                            callback_ref(state, id, &reload_key, &utf_sizes_key)
                        {
                            delivery.reload_actions.push(ReloadAction::Keep(callback));
                        } else {
                            delivery.reload_actions.push(ReloadAction::Detach(id));
                        }
                        continue;
                    }
                    if subscription.channel_id != 0 {
                        delivery.rpc_channels.push((id, subscription.channel_id));
                        continue;
                    }
                    if matches!(
                        &delivery.event.update,
                        BufferUpdateKind::Changedtick
                    ) {
                        if let Some(callback) =
                            callback_ref(state, id, &tick_key, &utf_sizes_key)
                        {
                            delivery.tick_refs.push(callback);
                        }
                        continue;
                    }
                    if !matches!(&delivery.event.update, BufferUpdateKind::Mutation { .. }) {
                        continue;
                    }
                    if let Some(callback) =
                        callback_ref(state, id, &line_key, &utf_sizes_key)
                    {
                        delivery.line_refs.push(callback);
                    }
                    if let Some(callback) =
                        callback_ref(state, id, &bytes_key, &utf_sizes_key)
                    {
                        delivery.byte_refs.push(callback);
                    }
                }
                if !delivery.line_refs.is_empty()
                    || !delivery.byte_refs.is_empty()
                    || !delivery.tick_refs.is_empty()
                    || !delivery.rpc_channels.is_empty()
                    || !delivery.reload_actions.is_empty()
                    || !delivery.reload_channels.is_empty()
                {
                    deliveries.push(delivery);
                }
            }
            if !deliveries.is_empty() {
                pending.deliveries.push(PendingDelivery {
                    buffer: handle,
                    events: deliveries,
                });
            }
        }
        pending
    })
}

/// Counts queued buffer updates and attachment releases without taking them.
/// The `vim.api` dispatch gate drains only when the call queued new work,
/// so read-only calls (a `parse` reading lines) never run user code while
/// borrowed userdata is live.
pub fn pending_buffer_bytes(session: &ApiSession) -> usize {
    session.with_editor(|editor| {
        editor.pending_subscription_releases_len()
            + editor
                .buffers()
                .iter()
                .filter_map(|handle| editor.buffer(*handle).ok())
                .map(BufferState::pending_callback_work_len)
                .sum::<usize>()
    })
}

/// Calls one stored Lua callback with integer arguments under the active
/// dispatch context's textlock and returns its value so the caller can decide
/// whether the truthy return detaches.
fn invoke_callback(
    lua: &Lua,
    context: &ApiDispatchContext,
    reference: i32,
    args: Vec<Value>,
) -> Result<Value, String> {
    let value =
        object_to_lua(lua, &Object::LuaRef(reference)).map_err(|error| error.to_string())?;
    let Value::Function(function) = value else {
        return Err("buffer callback reference is not a function".to_owned());
    };
    let _textlock_guard = context.enter_textlock();
    let _caller_guard = context.session().enter_internal_call();
    function
        .call::<Value>(Variadic::from_iter(args))
        .map_err(|error| error.to_string())
}

/// Converts a bounded editor integer to the API's signed integer shape.
fn api_integer(value: usize, what: &str) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{what} is out of range"))
}

/// Adds two editor coordinates without silently wrapping at the integer
/// boundary.
fn api_sum(left: usize, right: usize, what: &str) -> Result<i64, String> {
    api_integer(
        left.checked_add(right)
            .ok_or_else(|| format!("{what} is out of range"))?,
        what,
    )
}

/// Builds the linewise callback's arguments for one mutation.
fn lines_args(
    lua: &Lua,
    buffer: BufHandle,
    event: &BufferBytesEvent,
    utf_sizes: bool,
) -> Result<Vec<Value>, String> {
    let BufferUpdateKind::Mutation {
        old_line_count,
        old_byte_size,
        deleted_codepoints,
        deleted_codeunits,
        new_lines,
    } = &event.update
    else {
        return Err("line callback received a non-mutation event".to_owned());
    };
    let mut args = Vec::with_capacity(if utf_sizes { 9 } else { 7 });
    args.push(Value::String(
        lua.create_string("lines")
            .map_err(|error| error.to_string())?,
    ));
    args.push(Value::Integer(i64::from(buffer)));
    args.push(Value::Integer(
        i64::try_from(event.tick).map_err(|_| "buffer changedtick is out of range".to_owned())?,
    ));
    args.push(Value::Integer(api_integer(event.start_row, "buffer line")?));
    args.push(Value::Integer(api_sum(
        event.start_row,
        *old_line_count,
        "buffer line",
    )?));
    args.push(Value::Integer(api_sum(
        event.start_row,
        new_lines.len(),
        "buffer line",
    )?));
    args.push(Value::Integer(api_integer(*old_byte_size, "deleted byte size")?));
    if utf_sizes {
        args.push(Value::Integer(api_integer(
            *deleted_codepoints,
            "deleted UTF-32 size",
        )?));
        args.push(Value::Integer(api_integer(
            *deleted_codeunits,
            "deleted UTF-16 size",
        )?));
    }
    Ok(args)
}

/// Builds the three `on_changedtick` arguments for a tick-only update.
fn changedtick_args(
    lua: &Lua,
    buffer: BufHandle,
    event: &BufferBytesEvent,
) -> Result<Vec<Value>, String> {
    let mut args = Vec::with_capacity(3);
    args.push(Value::String(
        lua.create_string("changedtick")
            .map_err(|error| error.to_string())?,
    ));
    args.push(Value::Integer(i64::from(buffer)));
    args.push(Value::Integer(
        i64::try_from(event.tick).map_err(|_| "buffer changedtick is out of range".to_owned())?,
    ));
    Ok(args)
}

/// Builds the two `on_reload` arguments for a whole-buffer re-read.
fn reload_args(lua: &Lua, buffer: BufHandle) -> Result<Vec<Value>, String> {
    Ok(vec![
        Value::String(
            lua.create_string("reload")
                .map_err(|error| error.to_string())?,
        ),
        Value::Integer(i64::from(buffer)),
    ])
}

/// Builds the twelve `on_bytes` arguments for one event: the event name
/// first (upstream invokes Lua attach callbacks with the name prepended),
/// then buffer, tick, and the nine position integers.
fn bytes_args(
    lua: &Lua,
    buffer: BufHandle,
    event: &BufferBytesEvent,
) -> Result<Vec<Value>, String> {
    let mut args = Vec::with_capacity(12);
    args.push(Value::String(
        lua.create_string("bytes")
            .map_err(|error| error.to_string())?,
    ));
    args.push(Value::Integer(i64::from(buffer)));
    args.push(Value::Integer(
        i64::try_from(event.tick).map_err(|_| "buffer changedtick is out of range".to_owned())?,
    ));
    for value in [
        event.start_row,
        event.start_col,
        event.start_byte,
        event.old_row,
        event.old_col,
        event.old_byte,
        event.new_row,
        event.new_col,
        event.new_byte,
    ] {
        args.push(Value::Integer(
            i64::try_from(value).map_err(|_| "buffer byte offset is out of range".to_owned())?,
        ));
    }
    Ok(args)
}

/// Converts the post-edit line list into the wire array used by
/// `nvim_buf_lines_event`.
fn line_data(lines: &[Vec<u8>]) -> Object {
    Object::Array(
        lines
            .iter()
            .map(|line| Object::String(OxStr::from(line.as_slice())))
            .collect(),
    )
}

/// Projects one editor event onto the documented RPC notification shape.
fn rpc_message(buffer: BufHandle, event: &BufferBytesEvent) -> Result<Message, String> {
    let buffer = Object::Integer(i64::from(buffer));
    let changedtick = Object::Integer(
        i64::try_from(event.tick)
            .map_err(|_| "buffer changedtick is out of range".to_owned())?,
    );
    match &event.update {
        BufferUpdateKind::Mutation {
            old_line_count,
            new_lines,
            ..
        } => Ok(Message::Notification {
            method: OxStr::from("nvim_buf_lines_event"),
            params: vec![
                buffer,
                changedtick,
                Object::Integer(api_integer(event.start_row, "buffer line")?),
                Object::Integer(api_sum(
                    event.start_row,
                    *old_line_count,
                    "buffer line",
                )?),
                line_data(new_lines),
                Object::Boolean(false),
            ],
        }),
        BufferUpdateKind::Initial { new_lines } => Ok(Message::Notification {
            method: OxStr::from("nvim_buf_lines_event"),
            params: vec![
                buffer,
                changedtick,
                Object::Integer(0),
                Object::Integer(-1),
                line_data(new_lines),
                Object::Boolean(false),
            ],
        }),
        BufferUpdateKind::Changedtick => Ok(Message::Notification {
            method: OxStr::from("nvim_buf_changedtick_event"),
            params: vec![buffer, changedtick],
        }),
        BufferUpdateKind::Reload => {
            unreachable!("reload events are never routed to RPC channels");
        }
    }
}

/// Sends one encoded event through the public `nvim_chan_send` dispatch. The
/// channel sink is host code, so this function runs only after the editor
/// borrow used by `collect_pending` has ended.
fn send_rpc_event(
    dispatch: DispatchFn,
    session: &ApiSession,
    buffer: BufHandle,
    channel: u64,
    event: &BufferBytesEvent,
) -> Result<(), String> {
    let message = rpc_message(buffer, event)?
        .encode_bytes()
        .map_err(|error| error.to_string())?;
    let channel =
        i64::try_from(channel).map_err(|_| "buffer channel is out of range".to_owned())?;
    dispatch(
        session,
        &[
            Object::Integer(channel),
            Object::String(OxStr::from(message.as_slice())),
        ],
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Sends the end notification used when a reload drops an RPC attachment.
fn send_rpc_detach_event(
    dispatch: DispatchFn,
    session: &ApiSession,
    buffer: BufHandle,
    channel: u64,
) -> Result<(), String> {
    let message = Message::Notification {
        method: OxStr::from("nvim_buf_detach_event"),
        params: vec![Object::Integer(i64::from(buffer))],
    }
    .encode_bytes()
    .map_err(|error| error.to_string())?;
    let channel =
        i64::try_from(channel).map_err(|_| "buffer channel is out of range".to_owned())?;
    dispatch(
        session,
        &[
            Object::Integer(channel),
            Object::String(OxStr::from(message.as_slice())),
        ],
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Resolves the generated `nvim_chan_send` entry lazily, avoiding a registry
/// build on the common no-RPC-callback path.
fn rpc_dispatch() -> Result<DispatchFn, String> {
    let registry = ox_api::core().map_err(|error| error.to_string())?;
    registry
        .get("nvim_chan_send")
        .map(|(_, dispatch)| dispatch)
        .ok_or_else(|| "nvim_chan_send is not registered".to_owned())
}

/// True for every Lua value except `nil` and `false`, matching the
/// `nvim_buf_attach` truthy-detach contract.
fn is_truthy(value: Value) -> bool {
    !matches!(value, Value::Nil | Value::Boolean(false))
}

/// Delivers every pending reload, line, byte, and RPC event in commit order.
/// Remote notifications and reload-channel end messages are emitted before
/// Lua callbacks for each event, and line callbacks precede byte callbacks.
/// The loop continues until the queues stay empty, so a listener that edits
/// only observes settled state, matching upstream's synchronous nesting.
///
/// A truthy return from a line, byte, or changedtick callback removes that
/// subscription before any later event reaches it; `on_reload` is a
/// notification and ignores its return value. A subscription removed by
/// reentrant user code is skipped as soon as the drain notices. A transport
/// failure keeps only its undelivered recipient at the front of that buffer's
/// queue, preventing a successful sibling channel from receiving a duplicate
/// on retry.
///
/// # Errors
///
/// Returns the first listener, transport, or argument-shaping failure as a
/// string, or an error when no API dispatch context is registered for the Lua
/// host.
pub fn drain_buffer_callbacks(lua: &Lua, session: &ApiSession) -> Result<(), String> {
    let context = lua
        .app_data_ref::<ApiDispatchContext>()
        .map(|context| context.clone())
        .ok_or_else(|| {
            "buffer callback drain ran without a registered API dispatch context".to_owned()
        })?;
    let mut first_error: Option<String> = None;
    let mut detached: HashSet<u128> = HashSet::new();
    let mut channel_dispatch: Option<DispatchFn> = None;
    loop {
        let batch = collect_pending(session);
        if batch.deliveries.is_empty() && batch.released.is_empty() {
            break;
        }
        if let Err(error) = invoke_detach_callbacks(lua, &context, &batch.released) {
            first_error.get_or_insert(error);
        }
        if let Err(error) = release_removed_subscriptions(lua, &batch.released) {
            first_error.get_or_insert(error);
        }
        let mut retries: BTreeMap<BufHandle, Vec<BufferBytesEvent>> = BTreeMap::new();
        for pending in batch.deliveries {
            for event_delivery in pending.events {
                let mut reload_channels = Vec::new();
                session.with_editor(|editor| {
                    let Ok(state) = editor.buffer(pending.buffer) else {
                        return;
                    };
                    for (id, channel) in &event_delivery.reload_channels {
                        if state.subscriptions().contains_key(id) {
                            reload_channels.push((*id, *channel));
                        }
                    }
                });

                let mut failed_channels = Vec::new();
                let mut completed_reload_channels = Vec::new();
                for (id, channel) in &reload_channels {
                    let dispatch = match channel_dispatch {
                        Some(dispatch) => dispatch,
                        None => match rpc_dispatch() {
                            Ok(dispatch) => {
                                channel_dispatch = Some(dispatch);
                                dispatch
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                                failed_channels.push(*id);
                                continue;
                            }
                        },
                    };
                    if let Err(error) =
                        send_rpc_detach_event(dispatch, session, pending.buffer, *channel)
                    {
                        first_error.get_or_insert(error);
                        failed_channels.push(*id);
                    } else {
                        completed_reload_channels.push(*id);
                    }
                }
                if !completed_reload_channels.is_empty() {
                    session.with_editor_mut(|editor| {
                        if let Ok(state) = editor.buffer_mut(pending.buffer) {
                            for id in &completed_reload_channels {
                                state.remove_subscription_for_reload(*id);
                            }
                        }
                    });
                }
                for (id, channel) in &event_delivery.rpc_channels {
                    let dispatch = match channel_dispatch {
                        Some(dispatch) => dispatch,
                        None => match rpc_dispatch() {
                            Ok(dispatch) => {
                                channel_dispatch = Some(dispatch);
                                dispatch
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                                failed_channels.push(*id);
                                continue;
                            }
                        },
                    };
                    if let Err(error) = send_rpc_event(
                        dispatch,
                        session,
                        pending.buffer,
                        *channel,
                        &event_delivery.event,
                    ) {
                        first_error.get_or_insert(error);
                        failed_channels.push(*id);
                    }
                }

                for callback in &event_delivery.line_refs {
                    if detached.contains(&callback.id) {
                        continue;
                    }
                    let still_attached = session.with_editor(|editor| {
                        editor
                            .buffer(pending.buffer)
                            .is_ok_and(|state| state.subscriptions().contains_key(&callback.id))
                    });
                    if !still_attached {
                        detached.insert(callback.id);
                        continue;
                    }
                    let args = match lines_args(
                        lua,
                        pending.buffer,
                        &event_delivery.event,
                        callback.utf_sizes,
                    ) {
                        Ok(args) => args,
                        Err(error) => {
                            first_error.get_or_insert(error);
                            continue;
                        }
                    };
                    match invoke_callback(lua, &context, callback.reference, args) {
                        Ok(value) => {
                            if is_truthy(value) {
                                detached.insert(callback.id);
                                session.with_editor_mut(|editor| {
                                    if let Ok(state) = editor.buffer_mut(pending.buffer) {
                                        state.remove_subscription(callback.id);
                                    }
                                });
                            }
                        }
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }

                let bytes = if event_delivery.byte_refs.is_empty() {
                    None
                } else {
                    match bytes_args(lua, pending.buffer, &event_delivery.event) {
                        Ok(args) => Some(args),
                        Err(error) => {
                            first_error.get_or_insert(error);
                            None
                        }
                    }
                };
                if let Some(args) = bytes {
                    for callback in &event_delivery.byte_refs {
                        if detached.contains(&callback.id) {
                            continue;
                        }
                        let still_attached = session.with_editor(|editor| {
                            editor
                                .buffer(pending.buffer)
                                .is_ok_and(|state| {
                                    state.subscriptions().contains_key(&callback.id)
                                })
                        });
                        if !still_attached {
                            detached.insert(callback.id);
                            continue;
                        }
                        match invoke_callback(lua, &context, callback.reference, args.clone()) {
                            Ok(value) => {
                                if is_truthy(value) {
                                    detached.insert(callback.id);
                                    session.with_editor_mut(|editor| {
                                        if let Ok(state) = editor.buffer_mut(pending.buffer) {
                                            state.remove_subscription(callback.id);
                                        }
                                    });
                                }
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                            }
                        }
                    }
                }

                for callback in &event_delivery.tick_refs {
                    if detached.contains(&callback.id) {
                        continue;
                    }
                    let still_attached = session.with_editor(|editor| {
                        editor
                            .buffer(pending.buffer)
                            .is_ok_and(|state| state.subscriptions().contains_key(&callback.id))
                    });
                    if !still_attached {
                        detached.insert(callback.id);
                        continue;
                    }
                    let args = match changedtick_args(
                        lua,
                        pending.buffer,
                        &event_delivery.event,
                    ) {
                        Ok(args) => args,
                        Err(error) => {
                            first_error.get_or_insert(error);
                            continue;
                        }
                    };
                    match invoke_callback(lua, &context, callback.reference, args) {
                        Ok(value) => {
                            if is_truthy(value) {
                                detached.insert(callback.id);
                                session.with_editor_mut(|editor| {
                                    if let Ok(state) = editor.buffer_mut(pending.buffer) {
                                        state.remove_subscription(callback.id);
                                    }
                                });
                            }
                        }
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                }
                for action in &event_delivery.reload_actions {
                    match action {
                        ReloadAction::Detach(id) => {
                            let release = session.with_editor_mut(|editor| {
                                let state = editor.buffer_mut(pending.buffer).ok()?;
                                state
                                    .remove_subscription_for_reload(*id)
                                    .map(|subscription| BufferSubscriptionRelease {
                                        buffer: pending.buffer,
                                        subscription,
                                    })
                            });
                            detached.insert(*id);
                            let Some(release) = release else {
                                continue;
                            };
                            if let Err(error) =
                                invoke_detach_callbacks(lua, &context, std::slice::from_ref(&release))
                            {
                                first_error.get_or_insert(error);
                            }
                            if let Err(error) =
                                release_removed_subscriptions(lua, std::slice::from_ref(&release))
                            {
                                first_error.get_or_insert(error);
                            }
                        }
                        ReloadAction::Keep(callback) => {
                            if detached.contains(&callback.id) {
                                continue;
                            }
                            let still_attached = session.with_editor(|editor| {
                                editor
                                    .buffer(pending.buffer)
                                    .is_ok_and(|state| {
                                        state.subscriptions().contains_key(&callback.id)
                                    })
                            });
                            if !still_attached {
                                detached.insert(callback.id);
                                continue;
                            }
                            let args = match reload_args(lua, pending.buffer) {
                                Ok(args) => args,
                                Err(error) => {
                                    first_error.get_or_insert(error);
                                    continue;
                                }
                            };
                            // `on_reload` is a notification, not a detachable
                            // update callback: upstream keeps this subscription
                            // regardless of the callback's return value.
                            match invoke_callback(lua, &context, callback.reference, args) {
                                Ok(_) => {}
                                Err(error) => {
                                    first_error.get_or_insert(error);
                                }
                            }
                        }
                    }
                }

                if !failed_channels.is_empty() {
                    let mut event = event_delivery.event;
                    event
                        .subscribers
                        .retain(|id| failed_channels.contains(id));
                    if !event.subscribers.is_empty() {
                        retries.entry(pending.buffer).or_default().push(event);
                    }
                }
            }
        }
        if retries.is_empty() {
            continue;
        }
        session.with_editor_mut(|editor| {
            for (buffer, events) in retries {
                if let Ok(state) = editor.buffer_mut(buffer) {
                    state.prepend_bytes_events(events);
                }
            }
        });
        break;
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use mlua::Table;
    use ox_api::{register_channel, set_channel_sink, ChannelInfo, ChannelSink};
    use ox_rpc::{ChannelId, IncrementalDecoder};
    use ox_editor::{BufferAttachSubscription, Editor, Geometry};
    use ox_types::Dict;

    use super::*;
    use crate::converter::lua_to_object_ref;

    fn live_lua_ref_count(lua: &Lua) -> usize {
        let Ok(Some(refs)) = lua.named_registry_value::<Option<Table>>("ox-lua.refs") else {
            return 0;
        };
        let next: i32 = refs.raw_get("__next").unwrap();
        (1..next)
            .filter(|reference| {
                !matches!(
                    refs.raw_get::<Value>(*reference).unwrap(),
                    Value::Nil
                )
            })
            .count()
    }
    fn setup_with_callback<F>(lua: &Lua, callback: F) -> (Editor, BufHandle, i32)
    where
        F: Fn(&Lua, Variadic<Value>) -> Result<Value, mlua::Error> + 'static,
    {
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();

        let lua_callback = lua
            .create_function(move |lua, args: Variadic<Value>| callback(lua, args))
            .unwrap();
        let Object::LuaRef(reference) =
            lua_to_object_ref(lua, &Value::Function(lua_callback)).unwrap()
        else {
            unreachable!("a Lua function stores as a LuaRef")
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();
        state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(reference))]),
        });
        state
            .replace_lines(1, 1, &[b"first".to_vec(), b"second".to_vec()], cursor, cursor, 0)
            .unwrap();

        (editor, buffer, reference)
    }


    fn callback_args(args: &Variadic<Value>) -> Vec<String> {
        args.iter()
            .map(|value| match value {
                Value::Integer(integer) => integer.to_string(),
                Value::String(text) => text.to_string_lossy(),
                other => format!("{other:?}"),
            })
            .collect()
    }
    /// Low-level callback fixtures do not bind `vim.api`; provide the
    /// mandatory dispatch context without coupling their editor fixture to
    /// the API-binding integration setup.
    fn drain_test_callbacks(lua: &Lua, session: &ApiSession) -> Result<(), String> {
        if lua.app_data_ref::<ApiDispatchContext>().is_some() {
            return drain_buffer_callbacks(lua, session);
        }
        let context_session =
            Rc::new(ApiSession::new(Rc::new(RefCell::new(Editor::new()))));
        lua.set_app_data(ApiDispatchContext::new(context_session));
        drain_buffer_callbacks(lua, session)
    }

    /// `nvim_buf_attach` with `on_bytes` delivers one twelve-argument call
    /// per committed splice, in commit order.
    #[test]
    fn drain_delivers_bytes_events_in_commit_order() {
        let lua = Lua::new();
        let calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let seen = Rc::clone(&calls);

        let callback = move |_: &Lua, args: Variadic<Value>| {
            let mut record = Vec::with_capacity(args.len());
            for value in args.iter() {
                record.push(match value {
                    Value::Integer(integer) => integer.to_string(),
                    Value::String(text) => text.to_string_lossy(),
                    other => format!("{other:?}"),
                });
            }
            seen.borrow_mut().push(record);
            Ok(Value::Nil)
        };

        let (mut editor, buffer, _) = setup_with_callback(&lua, callback);
        let window = editor.current_window().unwrap();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();
        state
            .replace_lines(2, 2, &[b"changed".to_vec()], cursor, cursor, 0)
            .unwrap();

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        assert_eq!(pending_buffer_bytes(&session), 2);
        drain_test_callbacks(&lua, &session).unwrap();
        assert_eq!(pending_buffer_bytes(&session), 0);

        let calls = calls.borrow();
        assert_eq!(calls.len(), 2);
        let bufnr = i64::from(buffer).to_string();
        for call in calls.iter() {
            assert_eq!(call.len(), 12);
            assert_eq!(call[0], "bytes");
            assert_eq!(call[1], bufnr);
            assert_eq!(call[4], "0");
        }
        assert_eq!(calls[0][3], "0");
        assert_eq!(calls[1][3], "1");
        let first_tick: i64 = calls[0][2].parse().unwrap();
        let second_tick: i64 = calls[1][2].parse().unwrap();
        assert!(first_tick < second_tick);
    }

    /// A whole-buffer re-read invokes `on_reload` without replaying a line or
    /// byte delta, and a truthy return does not detach the subscription.
    #[test]
    fn reload_delivers_callback_without_delta_and_keeps_attachment() {
        let lua = Lua::new();
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();

        let reload_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let line_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let byte_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));

        let reload_seen = Rc::clone(&reload_calls);
        let reload_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                reload_seen.borrow_mut().push(callback_args(&args));
                // `on_reload` is a notification; upstream keeps the
                // subscription regardless of a truthy callback result.
                Ok(Value::Boolean(true))
            })
            .unwrap();
        let line_seen = Rc::clone(&line_calls);
        let line_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                line_seen.borrow_mut().push(callback_args(&args));
                Ok(Value::Nil)
            })
            .unwrap();
        let byte_seen = Rc::clone(&byte_calls);
        let byte_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                byte_seen.borrow_mut().push(callback_args(&args));
                Ok(Value::Nil)
            })
            .unwrap();

        let Object::LuaRef(reload_ref) =
            lua_to_object_ref(&lua, &Value::Function(reload_callback)).unwrap()
        else {
            unreachable!()
        };
        let Object::LuaRef(line_ref) =
            lua_to_object_ref(&lua, &Value::Function(line_callback)).unwrap()
        else {
            unreachable!()
        };
        let Object::LuaRef(byte_ref) =
            lua_to_object_ref(&lua, &Value::Function(byte_callback)).unwrap()
        else {
            unreachable!()
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();
        let id = state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![
                (OxStr::from("on_reload"), Object::LuaRef(reload_ref)),
                (OxStr::from("on_lines"), Object::LuaRef(line_ref)),
                (OxStr::from("on_bytes"), Object::LuaRef(byte_ref)),
            ]),
        });
        // Make the re-read replace different text and discard its earlier
        // mutation event; the reload must still carry no line/byte delta.
        let replacement = state.text().unwrap().clone();
        state
            .replace_lines(1, 1, &[b"before".to_vec()], cursor, cursor, 0)
            .unwrap();
        assert_eq!(state.take_bytes_events().len(), 1);
        state.load(replacement);

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        assert_eq!(pending_buffer_bytes(&session), 1);
        drain_test_callbacks(&lua, &session).unwrap();
        assert_eq!(pending_buffer_bytes(&session), 0);

        {
            let reload_calls = reload_calls.borrow();
            assert_eq!(reload_calls.len(), 1);
            assert_eq!(
                reload_calls[0],
                vec!["reload".to_owned(), i64::from(buffer).to_string()]
            );
        }
        assert!(line_calls.borrow().is_empty());
        assert!(byte_calls.borrow().is_empty());
        assert!(session.with_editor(|editor| {
            editor
                .buffer(buffer)
                .unwrap()
                .subscriptions()
                .contains_key(&id)
        }));

        session.with_editor_mut(|editor| {
            let state = editor.buffer_mut(buffer).unwrap();
            state
                .replace_lines(1, 1, &[b"after".to_vec()], cursor, cursor, 0)
                .unwrap();
        });
        drain_test_callbacks(&lua, &session).unwrap();
        assert_eq!(reload_calls.borrow().len(), 1);
        assert_eq!(line_calls.borrow().len(), 1);
        assert_eq!(byte_calls.borrow().len(), 1);
    }

    /// A Lua attachment without `on_reload` receives `on_detach` and no
    /// longer observes mutations after the buffer is re-read.
    #[test]
    fn reload_detaches_lua_attachment_without_on_reload() {
        let lua = Lua::new();
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();

        let detach_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let byte_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let detach_seen = Rc::clone(&detach_calls);
        let detach_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                detach_seen.borrow_mut().push(callback_args(&args));
                Ok(Value::Nil)
            })
            .unwrap();
        let byte_seen = Rc::clone(&byte_calls);
        let byte_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                byte_seen.borrow_mut().push(callback_args(&args));
                Ok(Value::Nil)
            })
            .unwrap();
        let Object::LuaRef(detach_ref) =
            lua_to_object_ref(&lua, &Value::Function(detach_callback)).unwrap()
        else {
            unreachable!()
        };
        let Object::LuaRef(byte_ref) =
            lua_to_object_ref(&lua, &Value::Function(byte_callback)).unwrap()
        else {
            unreachable!()
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();
        let id = state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![
                (OxStr::from("on_detach"), Object::LuaRef(detach_ref)),
                (OxStr::from("on_bytes"), Object::LuaRef(byte_ref)),
            ]),
        });
        let replacement = state.text().unwrap().clone();
        state
            .replace_lines(1, 1, &[b"before".to_vec()], cursor, cursor, 0)
            .unwrap();
        assert_eq!(state.take_bytes_events().len(), 1);
        state.load(replacement);

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        drain_test_callbacks(&lua, &session).unwrap();
        {
            let detach_calls = detach_calls.borrow();
            assert_eq!(detach_calls.len(), 1);
            assert_eq!(
                detach_calls[0],
                vec!["detach".to_owned(), i64::from(buffer).to_string()]
            );
        }
        assert!(byte_calls.borrow().is_empty());
        assert!(session.with_editor(|editor| {
            !editor
                .buffer(buffer)
                .unwrap()
                .subscriptions()
                .contains_key(&id)
        }));

        session.with_editor_mut(|editor| {
            let state = editor.buffer_mut(buffer).unwrap();
            state
                .replace_lines(1, 1, &[b"after".to_vec()], cursor, cursor, 0)
                .unwrap();
        });
        drain_test_callbacks(&lua, &session).unwrap();
        assert!(byte_calls.borrow().is_empty());
    }

    /// A second Lua attachment on the same buffer does not replace the first.
    #[test]
    fn two_distinct_lua_attachments_both_receive_events() {
        let lua = Lua::new();
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();

        let calls_a = Rc::new(RefCell::new(Vec::new()));
        let calls_b = Rc::new(RefCell::new(Vec::new()));

        let make_callback = |sink: Rc<RefCell<Vec<Vec<String>>>>| {
            move |_: &Lua, args: Variadic<Value>| {
                let record = args
                    .iter()
                    .map(|value| match value {
                        Value::Integer(i) => i.to_string(),
                        Value::String(s) => s.to_string_lossy(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                sink.borrow_mut().push(record);
                Ok(Value::Nil)
            }
        };

        let callback_a = lua
            .create_function(make_callback(Rc::clone(&calls_a)))
            .unwrap();
        let callback_b = lua
            .create_function(make_callback(Rc::clone(&calls_b)))
            .unwrap();

        let Object::LuaRef(ref_a) =
            lua_to_object_ref(&lua, &Value::Function(callback_a)).unwrap()
        else {
            unreachable!()
        };
        let Object::LuaRef(ref_b) =
            lua_to_object_ref(&lua, &Value::Function(callback_b)).unwrap()
        else {
            unreachable!()
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();

        let id_a = state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_a))]),
        });
        let id_b = state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_b))]),
        });
        assert_ne!(id_a, id_b);

        state
            .replace_lines(1, 1, &[b"only".to_vec()], cursor, cursor, 0)
            .unwrap();

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        drain_test_callbacks(&lua, &session).unwrap();

        assert_eq!(calls_a.borrow().len(), 1);
        assert_eq!(calls_b.borrow().len(), 1);
        assert_eq!(calls_a.borrow()[0][3], "0");
        assert_eq!(calls_b.borrow()[0][3], "0");
    }

    /// A listener that attaches while events are queued receives only the
    /// edits that happen after its attachment, not the earlier queued ones.
    #[test]
    fn new_listener_does_not_receive_pre_attachment_events() {
        let lua = Lua::new();
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();

        let calls_a = Rc::new(RefCell::new(Vec::new()));
        let calls_b = Rc::new(RefCell::new(Vec::new()));

        let make_callback = |sink: Rc<RefCell<Vec<Vec<String>>>>| {
            move |_: &Lua, args: Variadic<Value>| {
                let record = args
                    .iter()
                    .map(|value| match value {
                        Value::Integer(i) => i.to_string(),
                        Value::String(s) => s.to_string_lossy(),
                        other => format!("{other:?}"),
                    })
                    .collect();
                sink.borrow_mut().push(record);
                Ok(Value::Nil)
            }
        };

        let callback_a = lua
            .create_function(make_callback(Rc::clone(&calls_a)))
            .unwrap();

        let Object::LuaRef(ref_a) =
            lua_to_object_ref(&lua, &Value::Function(callback_a)).unwrap()
        else {
            unreachable!()
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();

        let _id_a = state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_a))]),
        });

        // Queue the first edit before the second listener attaches.
        state
            .replace_lines(
                1,
                1,
                &[b"before".to_vec(), b"".to_vec()],
                cursor,
                cursor,
                0,
            )
            .unwrap();

        let callback_b = lua
            .create_function(make_callback(Rc::clone(&calls_b)))
            .unwrap();
        let Object::LuaRef(ref_b) =
            lua_to_object_ref(&lua, &Value::Function(callback_b)).unwrap()
        else {
            unreachable!()
        };

        let _id_b = state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_b))]),
        });

        // Queue the second edit after the second listener attaches.
        state
            .replace_lines(2, 2, &[b"after".to_vec()], cursor, cursor, 0)
            .unwrap();

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        drain_test_callbacks(&lua, &session).unwrap();

        assert_eq!(calls_a.borrow().len(), 2);
        assert_eq!(calls_b.borrow().len(), 1);
        assert_eq!(calls_b.borrow()[0][3], "1");
    }

    /// A truthy callback return detaches that one subscription while the
    /// other keeps receiving later events.
    #[test]
    fn truthy_return_detaches_only_owning_subscription() {
        let lua = Lua::new();
        let baseline = live_lua_ref_count(&lua);
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();

        let calls_a = Rc::new(RefCell::new(0usize));
        let calls_b = Rc::new(RefCell::new(0usize));

        let cb_a = {
            let count = Rc::clone(&calls_a);
            move |_: &Lua, _args: Variadic<Value>| {
                *count.borrow_mut() += 1;
                // Detach after the first event.
                Ok(Value::Boolean(true))
            }
        };
        let cb_b = {
            let count = Rc::clone(&calls_b);
            move |_: &Lua, _args: Variadic<Value>| {
                *count.borrow_mut() += 1;
                Ok(Value::Nil)
            }
        };

        let f_a = lua.create_function(cb_a).unwrap();
        let f_b = lua.create_function(cb_b).unwrap();
        let Object::LuaRef(ref_a) = lua_to_object_ref(&lua, &Value::Function(f_a)).unwrap() else {
            unreachable!()
        };
        let Object::LuaRef(ref_b) = lua_to_object_ref(&lua, &Value::Function(f_b)).unwrap() else {
            unreachable!()
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let state = editor.buffer_mut(buffer).unwrap();

        state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_a))]),
        });
        state.attach_lua(BufferAttachSubscription {
            channel_id: 0,
            send_buffer: false,
            options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_b))]),
        });
        state
            .replace_lines(1, 1, &[b"one".to_vec()], cursor, cursor, 0)
            .unwrap();
        state
            .replace_lines(1, 1, &[b"two".to_vec()], cursor, cursor, 0)
            .unwrap();

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        drain_test_callbacks(&lua, &session).unwrap();

        assert_eq!(*calls_a.borrow(), 1);
        assert_eq!(*calls_b.borrow(), 2);
        assert_eq!(live_lua_ref_count(&lua), baseline + 1);
        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .remove_subscriptions_by_channel(0);
        });
        drain_test_callbacks(&lua, &session).unwrap();
        assert_eq!(live_lua_ref_count(&lua), baseline);
    }

    #[test]
    fn detached_subscriptions_release_registry_refs() {
        let lua = Lua::new();
        let baseline = live_lua_ref_count(&lua);
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let session = ApiSession::new(Rc::new(RefCell::new(editor)));

        for _ in 0..8 {
            let callback = lua.create_function(|_, _args: Variadic<Value>| Ok(Value::Nil)).unwrap();
            let Object::LuaRef(reference) =
                lua_to_object_ref(&lua, &Value::Function(callback)).unwrap()
            else {
                unreachable!()
            };
            session.with_editor_mut(|editor| {
                editor
                    .buffer_mut(buffer)
                    .unwrap()
                    .attach_lua(BufferAttachSubscription {
                        channel_id: 0,
                        send_buffer: false,
                        options: Dict(vec![(
                            OxStr::from("on_bytes"),
                            Object::LuaRef(reference),
                        )]),
                    });
                editor
                    .buffer_mut(buffer)
                    .unwrap()
                    .remove_subscriptions_by_channel(0);
            });
            drain_test_callbacks(&lua, &session).unwrap();
            assert_eq!(live_lua_ref_count(&lua), baseline);
        }
    }

    #[test]
    fn unloading_buffer_releases_registry_refs() {
        let lua = Lua::new();
        let baseline = live_lua_ref_count(&lua);
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let callback = lua.create_function(|_, _args: Variadic<Value>| Ok(Value::Nil)).unwrap();
        let Object::LuaRef(reference) =
            lua_to_object_ref(&lua, &Value::Function(callback)).unwrap()
        else {
            unreachable!()
        };
        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .attach_lua(BufferAttachSubscription {
                    channel_id: 0,
                    send_buffer: false,
                    options: Dict(vec![(
                        OxStr::from("on_bytes"),
                        Object::LuaRef(reference),
                    )]),
                });
            editor.unload_buffer(buffer).unwrap();
        });

        assert_eq!(live_lua_ref_count(&lua), baseline + 1);
        drain_test_callbacks(&lua, &session).unwrap();
        assert_eq!(live_lua_ref_count(&lua), baseline);
    }

    #[test]
    fn consuming_wiped_state_returns_active_and_pending_subscriptions() {
        let lua = Lua::new();
        let baseline = live_lua_ref_count(&lua);
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let callback_a =
            lua.create_function(|_, _args: Variadic<Value>| Ok(Value::Nil)).unwrap();
        let callback_b =
            lua.create_function(|_, _args: Variadic<Value>| Ok(Value::Nil)).unwrap();
        let Object::LuaRef(ref_a) =
            lua_to_object_ref(&lua, &Value::Function(callback_a)).unwrap()
        else {
            unreachable!()
        };
        let Object::LuaRef(ref_b) =
            lua_to_object_ref(&lua, &Value::Function(callback_b)).unwrap()
        else {
            unreachable!()
        };
        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        session.with_editor_mut(|editor| {
            {
                let state = editor.buffer_mut(buffer).unwrap();
                state.attach_lua(BufferAttachSubscription {
                    channel_id: 0,
                    send_buffer: false,
                    options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_a))]),
                });
                state.remove_subscription(u128::from(u64::MAX) + 1);
                state.attach_lua(BufferAttachSubscription {
                    channel_id: 0,
                    send_buffer: false,
                    options: Dict(vec![(OxStr::from("on_bytes"), Object::LuaRef(ref_b))]),
                });
            }
            editor.wipe_buffer(buffer).unwrap();
        });

        assert_eq!(live_lua_ref_count(&lua), baseline + 2);
        drain_test_callbacks(&lua, &session).unwrap();

        assert_eq!(live_lua_ref_count(&lua), baseline);
    }
    #[test]
    fn lines_callback_precedes_bytes_callback_and_reports_utf_sizes() {
        let lua = Lua::new();
        lua.globals()
            .set("vim", lua.create_table().unwrap())
            .unwrap();
        let order = Rc::new(RefCell::new(Vec::<String>::new()));
        let line_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let byte_calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));

        let lines_order = Rc::clone(&order);
        let lines_seen = Rc::clone(&line_calls);
        let lines_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                lines_order.borrow_mut().push("lines".to_owned());
                lines_seen.borrow_mut().push(
                    args.iter()
                        .map(|value| match value {
                            Value::Integer(integer) => integer.to_string(),
                            Value::String(text) => text.to_string_lossy(),
                            other => format!("{other:?}"),
                        })
                        .collect(),
                );
                Ok(Value::Nil)
            })
            .unwrap();
        let bytes_order = Rc::clone(&order);
        let bytes_seen = Rc::clone(&byte_calls);
        let bytes_callback = lua
            .create_function(move |_: &Lua, args: Variadic<Value>| {
                bytes_order.borrow_mut().push("bytes".to_owned());
                bytes_seen.borrow_mut().push(
                    args.iter()
                        .map(|value| match value {
                            Value::Integer(integer) => integer.to_string(),
                            Value::String(text) => text.to_string_lossy(),
                            other => format!("{other:?}"),
                        })
                        .collect(),
                );
                Ok(Value::Nil)
            })
            .unwrap();
        let Object::LuaRef(lines_ref) =
            lua_to_object_ref(&lua, &Value::Function(lines_callback)).unwrap()
        else {
            unreachable!()
        };
        let Object::LuaRef(bytes_ref) =
            lua_to_object_ref(&lua, &Value::Function(bytes_callback)).unwrap()
        else {
            unreachable!()
        };

        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        editor
            .buffer_mut(buffer)
            .unwrap()
            .attach_lua(BufferAttachSubscription {
                channel_id: 0,
                send_buffer: false,
                options: Dict(vec![
                    (OxStr::from("on_lines"), Object::LuaRef(lines_ref)),
                    (OxStr::from("on_bytes"), Object::LuaRef(bytes_ref)),
                    (OxStr::from("utf_sizes"), Object::Boolean(true)),
                ]),
            });
        editor
            .buffer_mut(buffer)
            .unwrap()
            .replace_lines(1, 1, &["hé".as_bytes().to_vec()], cursor, cursor, 0)
            .unwrap();

        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        drain_test_callbacks(&lua, &session).unwrap();
        assert_eq!(*order.borrow(), vec!["lines", "bytes"]);

        let line = &line_calls.borrow()[0];
        assert_eq!(line.len(), 9);
        assert_eq!(line[0], "lines");
        assert_eq!(line[1], i64::from(buffer).to_string());
        assert_eq!(line[3], "0");
        assert_eq!(line[4], "1");
        assert_eq!(line[5], "1");
        assert_eq!(line[6], "1");
        assert_eq!(line[7], "1");
        assert_eq!(line[8], "1");

        let byte = &byte_calls.borrow()[0];
        assert_eq!(byte.len(), 12);
        assert_eq!(byte[0], "bytes");
        assert_eq!(byte[1], i64::from(buffer).to_string());
    }

    struct RecordingChannelSink {
        writes: Rc<RefCell<Vec<(u64, Vec<u8>)>>>,
    }

    impl ChannelSink for RecordingChannelSink {
        fn send(&mut self, channel: u64, bytes: &[u8]) -> Result<(), String> {
            self.writes.borrow_mut().push((channel, bytes.to_vec()));
            Ok(())
        }
    }

    fn decode_recorded_message(bytes: &[u8]) -> Message {
        let mut decoder = IncrementalDecoder::new();
        let mut messages = decoder.feed(bytes).unwrap();
        assert_eq!(messages.len(), 1);
        messages.remove(0)
    }

    #[test]
    fn rpc_buffer_attachments_receive_initial_and_mutation_events() {
        let writes = Rc::new(RefCell::new(Vec::<(u64, Vec<u8>)>::new()));
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        editor
            .buffer_mut(buffer)
            .unwrap()
            .replace_lines(
                1,
                1,
                &[b"first".to_vec(), b"second".to_vec()],
                cursor,
                cursor,
                0,
            )
            .unwrap();
        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        register_channel(&session, ChannelInfo::socket_rpc(ChannelId::new(7))).unwrap();
        register_channel(&session, ChannelInfo::socket_rpc(ChannelId::new(8))).unwrap();
        set_channel_sink(
            &session,
            Box::new(RecordingChannelSink {
                writes: Rc::clone(&writes),
            }),
        );

        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .insert_subscription(
                    7,
                    BufferAttachSubscription {
                        channel_id: 7,
                        send_buffer: true,
                        options: Dict(Vec::new()),
                    },
                );
        });
        drain_test_callbacks(&Lua::new(), &session).unwrap();
        assert_eq!(writes.borrow().len(), 1);
        assert_eq!(writes.borrow()[0].0, 7);
        let Message::Notification { method, params } =
            decode_recorded_message(&writes.borrow()[0].1)
        else {
            unreachable!()
        };
        assert_eq!(method, OxStr::from("nvim_buf_lines_event"));
        assert_eq!(params.len(), 6);
        assert_eq!(params[0], Object::Integer(i64::from(buffer)));
        assert_eq!(params[2], Object::Integer(0));
        assert_eq!(params[3], Object::Integer(-1));
        assert_eq!(
            params[4],
            Object::Array(vec![
                Object::String(OxStr::from("first")),
                Object::String(OxStr::from("second")),
            ])
        );
        assert_eq!(params[5], Object::Boolean(false));

        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .insert_subscription(
                    8,
                    BufferAttachSubscription {
                        channel_id: 8,
                        send_buffer: false,
                        options: Dict(Vec::new()),
                    },
                );
        });
        drain_test_callbacks(&Lua::new(), &session).unwrap();
        assert_eq!(writes.borrow().len(), 2);
        assert_eq!(writes.borrow()[1].0, 8);
        let Message::Notification { method, params } =
            decode_recorded_message(&writes.borrow()[1].1)
        else {
            unreachable!()
        };
        assert_eq!(method, OxStr::from("nvim_buf_changedtick_event"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], Object::Integer(i64::from(buffer)));

        session.with_editor_mut(|editor| {
            let state = editor.buffer_mut(buffer).unwrap();
            state
                .replace_lines(
                    1,
                    1,
                    &[b"next".to_vec()],
                    cursor,
                    cursor,
                    0,
                )
                .unwrap();
        });
        drain_test_callbacks(&Lua::new(), &session).unwrap();
        assert_eq!(writes.borrow().len(), 4);
        for (index, channel) in [7, 8].into_iter().enumerate() {
            let (written_channel, bytes) = &writes.borrow()[index + 2];
            assert_eq!(*written_channel, channel);
            let Message::Notification { method, params } = decode_recorded_message(bytes) else {
                unreachable!()
            };
            assert_eq!(method, OxStr::from("nvim_buf_lines_event"));
            assert_eq!(params[0], Object::Integer(i64::from(buffer)));
            assert_eq!(params[2], Object::Integer(0));
            assert_eq!(params[3], Object::Integer(1));
            assert_eq!(
                params[4],
                Object::Array(vec![Object::String(OxStr::from("next"))])
            );
            assert_eq!(params[5], Object::Boolean(false));
        }
    }

    #[test]
    fn rpc_buffer_attachment_ends_on_reload() {
        let writes = Rc::new(RefCell::new(Vec::<(u64, Vec<u8>)>::new()));
        let mut editor = Editor::new();
        let buffer = editor.create_buffer(true).unwrap();
        let tab = editor
            .create_tabpage(buffer, Geometry::new(0, 0, 80, 24).unwrap())
            .unwrap();
        let window = editor.tabpage(tab).unwrap().current_window();
        let cursor = editor.window(window).unwrap().cursor;
        let session = ApiSession::new(Rc::new(RefCell::new(editor)));
        register_channel(&session, ChannelInfo::socket_rpc(ChannelId::new(17))).unwrap();
        set_channel_sink(
            &session,
            Box::new(RecordingChannelSink {
                writes: Rc::clone(&writes),
            }),
        );

        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .insert_subscription(
                    17,
                    BufferAttachSubscription {
                        channel_id: 17,
                        send_buffer: false,
                        options: Dict(Vec::new()),
                    },
                );
        });
        drain_test_callbacks(&Lua::new(), &session).unwrap();
        assert_eq!(writes.borrow().len(), 1);
        session.with_editor_mut(|editor| {
            let state = editor.buffer_mut(buffer).unwrap();
            let replacement = state.text().unwrap().clone();
            state.load(replacement);
        });
        drain_test_callbacks(&Lua::new(), &session).unwrap();

        assert_eq!(writes.borrow().len(), 2);
        assert_eq!(writes.borrow()[1].0, 17);
        let Message::Notification { method, params } =
            decode_recorded_message(&writes.borrow()[1].1)
        else {
            unreachable!()
        };
        assert_eq!(method, OxStr::from("nvim_buf_detach_event"));
        assert_eq!(params, vec![Object::Integer(i64::from(buffer))]);
        assert!(session.with_editor(|editor| {
            !editor
                .buffer(buffer)
                .unwrap()
                .subscriptions()
                .contains_key(&17)
        }));

        session.with_editor_mut(|editor| {
            let state = editor.buffer_mut(buffer).unwrap();
            state
                .replace_lines(1, 1, &[b"after".to_vec()], cursor, cursor, 0)
                .unwrap();
        });
        drain_test_callbacks(&Lua::new(), &session).unwrap();
        assert_eq!(writes.borrow().len(), 2);
    }
}
