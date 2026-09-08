//! Delivery of `nvim_buf_attach` Lua callbacks for committed text mutations.
//!
//! The editor records one [`BufferBytesEvent`] per committed splice in the
//! buffer (see `BufferState::take_bytes_events`); the callbacks themselves
//! are Lua registry references owned by this layer, so the drain lives
//! here. Callers drain at every transition into user Lua (chunk entries,
//! callback invocations, and after each `vim.api` dispatch): the collect
//! phase runs under the editor borrow and the invoke phase runs without
//! it, so user code can reenter while the queues stay consistent. A
//! failing listener does not starve the rest: the first error returns
//! after the drain completes, and the edit it observed already stands.

use mlua::{Lua, Value, Variadic};
use ox_api::ApiSession;
use ox_editor::{BufferBytesEvent, BufferState};
use ox_types::{Object, OxStr};

use crate::converter::object_to_lua;

/// One buffer's drained callbacks plus its events. Built under the editor
/// borrow; invoked after it is released.
struct PendingDelivery {
    bufnr: i64,
    bytes_refs: Vec<i32>,
    events: Vec<BufferBytesEvent>,
}

/// Registry references for one callback key across every subscription.
fn callback_refs(state: &BufferState, key: &str) -> Vec<i32> {
    let key = OxStr::from(key);
    state
        .subscriptions()
        .values()
        .filter_map(|subscription| subscription.options.get(&key))
        .filter_map(|object| match object {
            Object::LuaRef(reference) => Some(*reference),
            _ => None,
        })
        .collect()
}

/// Collects every pending event plus the callback references registered
/// for its buffer, emptying all queues.
fn collect_pending(session: &ApiSession) -> Vec<PendingDelivery> {
    session.with_editor_mut(|editor| {
        let mut pending = Vec::new();
        for handle in editor.buffers() {
            let Ok(state) = editor.buffer_mut(handle) else {
                continue;
            };
            let events = state.take_bytes_events();
            if events.is_empty() {
                continue;
            }
            pending.push(PendingDelivery {
                bufnr: i64::from(handle),
                bytes_refs: callback_refs(state, "on_bytes"),
                events,
            });
        }
        pending
    })
}

/// Counts queued mutation events across all buffers without taking them.
/// The `vim.api` dispatch gate drains only when the call queued new
/// events, so read-only calls (a `parse` reading lines) never run user
/// code while borrowed userdata is live.
pub fn pending_buffer_bytes(session: &ApiSession) -> usize {
    session.with_editor(|editor| {
        editor
            .buffers()
            .iter()
            .filter_map(|handle| editor.buffer(*handle).ok())
            .map(BufferState::pending_bytes_len)
            .sum()
    })
}

/// Calls one stored Lua callback with integer arguments.
fn invoke_callback(lua: &Lua, reference: i32, args: Vec<Value>) -> Result<(), String> {
    let value =
        object_to_lua(lua, &Object::LuaRef(reference)).map_err(|error| error.to_string())?;
    let Value::Function(function) = value else {
        return Err("buffer callback reference is not a function".to_owned());
    };
    function
        .call::<()>(Variadic::from_iter(args))
        .map_err(|error| error.to_string())
}

/// Builds the twelve `on_bytes` arguments for one event: the event name
/// first (upstream invokes Lua attach callbacks with the name prepended),
/// then buffer, tick, and the nine position integers.
fn bytes_args(
    lua: &Lua,
    delivery: &PendingDelivery,
    event: &BufferBytesEvent,
) -> Result<Vec<Value>, String> {
    let mut args = Vec::with_capacity(12);
    args.push(Value::String(
        lua.create_string("bytes")
            .map_err(|error| error.to_string())?,
    ));
    args.push(Value::Integer(delivery.bufnr));
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

/// Delivers every pending `on_bytes` callback in commit order. Loops
/// until the queues stay empty, so a listener that edits only observes
/// settled state, matching upstream's synchronous nesting. Continues
/// past a failing listener and reports the first error after the drain.
/// (`on_changedtick` never fires for these edits on the reference build,
/// so it stays unwired; `on_lines` needs line bodies the events do not
/// carry. Both are recorded gaps, not silent skips.)
///
/// # Errors
///
/// Returns the first listener or argument-shaping failure as a string.
pub fn drain_buffer_callbacks(lua: &Lua, session: &ApiSession) -> Result<(), String> {
    let mut first_error: Option<String> = None;
    loop {
        let batch = collect_pending(session);
        if batch.is_empty() {
            break;
        }
        for delivery in &batch {
            for event in &delivery.events {
                let args = match bytes_args(lua, delivery, event) {
                    Ok(args) => args,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        continue;
                    }
                };
                for reference in &delivery.bytes_refs {
                    if let Err(error) = invoke_callback(lua, *reference, args.clone()) {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}
