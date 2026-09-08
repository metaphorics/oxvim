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

use std::collections::HashSet;

use mlua::{Lua, Value, Variadic};
use ox_api::ApiSession;
use ox_editor::{BufferAttachSubscription, BufferBytesEvent, BufferState};
use ox_types::{BufHandle, Object, OxStr};

use crate::converter::{free_lua_ref, object_to_lua};

/// One subscription's Lua registry reference plus the identity that owns it.
///
/// The id is needed so a truthy callback return can detach exactly the
/// subscription that produced it, even if user code attaches or detaches
/// other subscriptions while the drain is running.
struct CallbackRef {
    id: u128,
    reference: i32,
}

/// One buffer's drained callbacks and events. Built under the editor borrow;
/// invoked after it is released.
struct PendingDelivery {
    buffer: BufHandle,
    events: Vec<EventDelivery>,
}

/// All callback deliveries and registry references released by one editor pull.
struct PendingBatch {
    deliveries: Vec<PendingDelivery>,
    released: Vec<BufferAttachSubscription>,
}

/// One queued event and the callback references that were live for it.
struct EventDelivery {
    event: BufferBytesEvent,
    refs: Vec<CallbackRef>,
}

/// Resolves the `on_bytes` Lua reference for a single subscription, if any.
fn callback_ref(state: &BufferState, id: u128, key: &OxStr) -> Option<CallbackRef> {
    let subscription = state.subscriptions().get(&id)?;
    match subscription.options.get(key)? {
        Object::LuaRef(reference) => Some(CallbackRef {
            id,
            reference: *reference,
        }),
        _ => None,
    }
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
    subscriptions: &[BufferAttachSubscription],
) -> Result<(), String> {
    let mut first_error = None;
    for subscription in subscriptions {
        if let Err(error) = release_subscription_refs(lua, subscription)
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
    let key = OxStr::from("on_bytes");
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
            let released = state.take_pending_subscription_releases();
            pending.released.extend(released);
            if events.is_empty() {
                continue;
            }
            let mut deliveries = Vec::with_capacity(events.len());
            for event in events {
                let refs: Vec<CallbackRef> = event
                    .subscribers
                    .iter()
                    .copied()
                    .filter_map(|id| callback_ref(state, id, &key))
                    .collect();
                if !refs.is_empty() {
                    deliveries.push(EventDelivery { event, refs });
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

/// Calls one stored Lua callback with integer arguments and returns its
/// value so the caller can decide whether the truthy return detaches.
fn invoke_callback(lua: &Lua, reference: i32, args: Vec<Value>) -> Result<Value, String> {
    let value =
        object_to_lua(lua, &Object::LuaRef(reference)).map_err(|error| error.to_string())?;
    let Value::Function(function) = value else {
        return Err("buffer callback reference is not a function".to_owned());
    };
    function
        .call::<Value>(Variadic::from_iter(args))
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
    args.push(Value::Integer(i64::from(delivery.buffer)));
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

/// True for every Lua value except `nil` and `false`, matching the
/// `nvim_buf_attach` truthy-detach contract.
fn is_truthy(value: Value) -> bool {
    !matches!(value, Value::Nil | Value::Boolean(false))
}

/// Delivers every pending `on_bytes` callback in commit order. Loops
/// until the queues stay empty, so a listener that edits only observes
/// settled state, matching upstream's synchronous nesting. Continues
/// past a failing listener and reports the first error after the drain.
/// A truthy callback return removes that one subscription before any
/// later event reaches it; a subscription removed by reentrant user code
/// is skipped as soon as the drain notices.
///
/// # Errors
///
/// Returns the first listener or argument-shaping failure as a string.
pub fn drain_buffer_callbacks(lua: &Lua, session: &ApiSession) -> Result<(), String> {
    let mut first_error: Option<String> = None;
    let mut detached: HashSet<u128> = HashSet::new();
    loop {
        let batch = collect_pending(session);
        if batch.deliveries.is_empty() && batch.released.is_empty() {
            break;
        }
        if let Err(error) = release_removed_subscriptions(lua, &batch.released) {
            first_error.get_or_insert(error);
        }
        for delivery in &batch.deliveries {
            for event_delivery in &delivery.events {
                let args = match bytes_args(lua, delivery, &event_delivery.event) {
                    Ok(args) => args,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        continue;
                    }
                };
                for callback in &event_delivery.refs {
                    if detached.contains(&callback.id) {
                        continue;
                    }
                    let still_attached = session.with_editor(|editor| {
                        editor
                            .buffer(delivery.buffer)
                            .is_ok_and(|state| state.subscriptions().contains_key(&callback.id))
                    });
                    if !still_attached {
                        detached.insert(callback.id);
                        continue;
                    }
                    match invoke_callback(lua, callback.reference, args.clone()) {
                        Ok(value) => {
                            if is_truthy(value) {
                                detached.insert(callback.id);
                                session.with_editor_mut(|editor| {
                                    if let Ok(state) = editor.buffer_mut(delivery.buffer) {
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
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use mlua::Table;
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
        drain_buffer_callbacks(&lua, &session).unwrap();
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
        drain_buffer_callbacks(&lua, &session).unwrap();

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
        drain_buffer_callbacks(&lua, &session).unwrap();

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
        drain_buffer_callbacks(&lua, &session).unwrap();

        assert_eq!(*calls_a.borrow(), 1);
        assert_eq!(*calls_b.borrow(), 2);
        assert_eq!(live_lua_ref_count(&lua), baseline + 1);
        session.with_editor_mut(|editor| {
            editor
                .buffer_mut(buffer)
                .unwrap()
                .remove_subscriptions_by_channel(0);
        });
        drain_buffer_callbacks(&lua, &session).unwrap();
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
            drain_buffer_callbacks(&lua, &session).unwrap();
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
        drain_buffer_callbacks(&lua, &session).unwrap();
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
        drain_buffer_callbacks(&lua, &session).unwrap();
        assert_eq!(live_lua_ref_count(&lua), baseline);
    }
}
