//! `vim.ui_attach` / `vim.ui_detach` (upstream `nlua_ui_attach` /
//! `nlua_ui_detach`, lua/executor.c:807-893): subscribe a Lua callback to
//! the semantic UI event stream filtered by ext-widget families.
//!
//! Upstream registers the callback with `ui_add_cb` and the event loop
//! invokes it asynchronously. The port mirrors that split: producers call
//! [`enqueue_ui_event`] at emission time, and the server invokes the
//! callbacks at a borrow-free boundary via [`deliver_pending_ui_events`],
//! because firing Lua synchronously inside the render-state borrow would
//! reenter `RefCell` borrows.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use mlua::Function;
use mlua::Function as StoredCallback;
use mlua::Lua;
use mlua::MultiValue;
use mlua::Table;
use mlua::Value;
use ox_types::Object;

use crate::ApiDispatchContext;
use crate::FastCallbackState;

/// The global ext-widget options `nlua_ui_attach` accepts
/// (`ui_ext_names[0..kUIGlobalCount]`, api/ui.h:12-18), plus the special
/// `set_cmdheight` flag.
const GLOBAL_EXT_OPTIONS: [&str; 5] = [
    "ext_cmdline",
    "ext_popupmenu",
    "ext_tabline",
    "ext_wildmenu",
    "ext_messages",
];

/// Maps an event name to the ext-widget family gating it, mirroring the
/// emitter's routing (ox-ui emitter.rs `route_chrome`).
fn family(event: &str) -> Option<&'static str> {
    if event.starts_with("msg_") {
        Some("ext_messages")
    } else if event.starts_with("cmdline_") {
        Some("ext_cmdline")
    } else if event.starts_with("popupmenu_") {
        Some("ext_popupmenu")
    } else if event.starts_with("tabline_") {
        Some("ext_tabline")
    } else if event.starts_with("wildmenu_") {
        Some("ext_wildmenu")
    } else {
        None
    }
}

struct UiCallback {
    function: StoredCallback,
    families: BTreeSet<&'static str>,
}

thread_local! {
    /// Namespace id → attached callback. One attachment per namespace,
    /// matching `ui_add_cb`'s ns-keyed registry.
    static CALLBACKS: RefCell<BTreeMap<i64, UiCallback>> =
        const { RefCell::new(BTreeMap::new()) };

    /// Events queued by producers, drained by the server between redraws.
    static PENDING: RefCell<Vec<(String, Vec<Object>)>> = const { RefCell::new(Vec::new()) };
}

/// Installs `vim.ui_attach` and `vim.ui_detach` on the `vim` table.
///
/// # Errors
///
/// Returns an error when the `vim` table or a native function cannot be
/// created (mlua failures only; argument errors raise as Lua errors).
pub fn bind_ui_events(
    lua: &Lua,
    context: &ApiDispatchContext,
    fast_state: &FastCallbackState,
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let attach_context = context.clone();
    let attach_fast = fast_state.clone();
    let attach = lua.create_function(
        move |lua, (ns_id, opts, callback): (i64, Table, Function)| {
            ui_attach(lua, &attach_context, &attach_fast, ns_id, &opts, callback)
        },
    )?;
    let detach_context = context.clone();
    let detach = lua.create_function(move |_lua, ns_id: i64| ui_detach(&detach_context, ns_id))?;
    vim.set("ui_attach", attach)?;
    vim.set("ui_detach", detach)
}

fn ui_attach(
    lua: &Lua,
    context: &ApiDispatchContext,
    fast_state: &FastCallbackState,
    ns_id: i64,
    opts: &Table,
    callback: Function,
) -> mlua::Result<()> {
    fast_state.guard("vim.ui_attach")?;

    if !ns_initialized(context, ns_id) {
        return Err(mlua::Error::runtime("invalid ns_id"));
    }
    let mut families = BTreeSet::new();
    let mut has_true = false;
    for pair in opts.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let name = match key {
            Value::String(text) => text.to_str().map_err(mlua::Error::external)?.to_owned(),
            _ => continue,
        };
        let enabled = !matches!(value, Value::Nil | Value::Boolean(false));
        if name == "set_cmdheight" {
            // Upstream refreshes the cmdline height (`ui_refresh_cmdheight`);
            // the port's chrome layout is computed per redraw, so the flag
            // needs no stored state.
            continue;
        }
        let Some(option) = GLOBAL_EXT_OPTIONS
            .iter()
            .find(|candidate| **candidate == name)
        else {
            return Err(mlua::Error::runtime(format!("Unexpected key: {name}")));
        };
        if enabled {
            has_true = true;
        }
        if enabled {
            // The option name is the family key: `family()` maps event
            // names to these options, so the registry stores them directly.
            families.insert(*option);
        }
    }
    if !has_true {
        return Err(mlua::Error::runtime(
            "opts table must contain at least one 'true' ext_widget",
        ));
    }
    let _ = lua;
    CALLBACKS.with(|callbacks| {
        callbacks.borrow_mut().insert(
            ns_id,
            UiCallback {
                function: callback,
                families,
            },
        );
    });
    Ok(())
}

fn ui_detach(context: &ApiDispatchContext, ns_id: i64) -> mlua::Result<()> {
    if !ns_initialized(context, ns_id) {
        return Err(mlua::Error::runtime("invalid ns_id"));
    }
    CALLBACKS.with(|callbacks| {
        callbacks.borrow_mut().remove(&ns_id);
    });
    Ok(())
}

/// Upstream `ns_initialized`: the id was allocated by
/// `nvim_create_namespace`. The port allocates in the session state
/// (`nvim_create_namespace`, ox-api extmark.rs), so that registry answers.
fn ns_initialized(context: &ApiDispatchContext, ns_id: i64) -> bool {
    u32::try_from(ns_id).is_ok_and(|id| context.session().namespace_is_initialized(id))
}

/// Queues one semantic UI event for the attached callbacks. Called by the
/// server at emission time; nothing here touches Lua, so it is safe inside
/// render-state borrows.
pub fn enqueue_ui_event(name: &str, args: Vec<Object>) {
    let Some(family) = family(name) else {
        return;
    };
    let matched = CALLBACKS.with(|callbacks| {
        callbacks
            .borrow()
            .values()
            .any(|callback| callback.families.contains(family))
    });
    if matched {
        PENDING.with(|pending| {
            pending.borrow_mut().push((name.to_owned(), args));
        });
    }
}

/// Invokes every queued event on its callbacks. Runs only at borrow-free
/// server boundaries.
///
/// # Errors
///
/// Returns the first callback failure verbatim (upstream aborts the event
/// callback chain on error).
pub fn deliver_pending_ui_events(lua: &Lua) -> mlua::Result<()> {
    let events = PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()));
    for (name, args) in events {
        let family = family(&name);
        let callbacks: Vec<StoredCallback> = CALLBACKS.with(|callbacks| {
            callbacks
                .borrow()
                .values()
                .filter(|callback| family.is_some_and(|f| callback.families.contains(f)))
                .map(|callback| callback.function.clone())
                .collect()
        });
        for function in callbacks {
            let mut values = Vec::with_capacity(args.len() + 1);
            values.push(Value::String(lua.create_string(name.as_bytes())?));
            for arg in &args {
                values.push(crate::object_to_lua(lua, arg).map_err(mlua::Error::external)?);
            }
            function.call::<()>(MultiValue::from_iter(values))?;
        }
    }
    Ok(())
}

/// True when any callback is attached (the emitter may skip work otherwise).
#[must_use]
pub fn has_attached_callbacks() -> bool {
    CALLBACKS.with(|callbacks| !callbacks.borrow().is_empty())
}

/// Clears all attachments and queued events (session teardown).
pub fn reset() {
    CALLBACKS.with(|callbacks| callbacks.borrow_mut().clear());
    PENDING.with(|pending| pending.borrow_mut().clear());
}
