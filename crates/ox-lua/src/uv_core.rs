//! Behavior-critical `vim.uv` bindings shared by the complete UV adapter.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Instant;

use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};
use ox_uv::{CallbackError, Handle, RunMode, Timer, UvLoop};

use crate::host::RuntimeRoot;
use crate::vim::{call_with_traceback, BuiltinHost, FastCallbackState, Scheduler};

struct CoreState {
    files: RefCell<HashMap<i64, File>>,
    next_file: RefCell<i64>,
}

impl CoreState {
    fn new() -> Self {
        Self { files: RefCell::new(HashMap::new()), next_file: RefCell::new(3) }
    }

    fn insert_file(&self, file: File) -> i64 {
        let mut next = self.next_file.borrow_mut();
        let descriptor = *next;
        *next = next.saturating_add(1);
        self.files.borrow_mut().insert(descriptor, file);
        descriptor
    }
}

#[derive(Clone)]
struct LuaTimer {
    timer: Timer,
    uv_loop: Rc<RefCell<UvLoop>>,
}

impl UserData for LuaTimer {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "start",
            |lua, this, (timeout, repeat, callback): (u64, u64, Function)| {
                let lua = lua.clone();
                let context = lua
                    .app_data_ref::<CallbackContext>()
                    .ok_or_else(|| mlua::Error::runtime("vim.uv callback context is unavailable"))?;
                let scheduler = context.scheduler.clone();
                let fast = context.fast.clone();
                drop(context);
                this.timer
                    .start(&mut this.uv_loop.borrow_mut(), timeout, repeat, move |_, _| {
                        schedule_callback(
                            scheduler.clone(),
                            lua.clone(),
                            callback.clone(),
                            MultiValue::new(),
                            fast.clone(),
                        )
                        .map_err(CallbackError::new)
                    })
                    .map_err(mlua::Error::external)?;
                Ok(true)
            },
        );
        methods.add_method("stop", |_, this, ()| {
            this.timer.stop(&mut this.uv_loop.borrow_mut()).map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("again", |_, this, ()| {
            this.timer.again(&mut this.uv_loop.borrow_mut()).map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("set_repeat", |_, this, repeat: u64| {
            this.timer
                .set_repeat(&mut this.uv_loop.borrow_mut(), repeat)
                .map_err(mlua::Error::external)?;
            Ok(())
        });
        methods.add_method("get_repeat", |_, this, ()| {
            this.timer.get_repeat(&this.uv_loop.borrow()).map_err(mlua::Error::external)
        });
        methods.add_method("ref", |_, this, ()| {
            this.timer.ref_(&mut this.uv_loop.borrow_mut()).map_err(mlua::Error::external)?;
            Ok(this.clone())
        });
        methods.add_method("unref", |_, this, ()| {
            this.timer.unref(&mut this.uv_loop.borrow_mut()).map_err(mlua::Error::external)?;
            Ok(this.clone())
        });
        methods.add_method("has_ref", |_, this, ()| {
            Ok(this.timer.has_ref(&this.uv_loop.borrow()))
        });
        methods.add_method("is_active", |_, this, ()| {
            Ok(this.timer.is_active(&this.uv_loop.borrow()))
        });
        methods.add_method("is_closing", |_, this, ()| {
            Ok(this.timer.is_closing(&this.uv_loop.borrow()))
        });
        methods.add_method("close", |lua, this, callback: Option<Function>| {
            match callback {
                Some(callback) => {
                    let lua = lua.clone();
                    let context = lua
                        .app_data_ref::<CallbackContext>()
                        .ok_or_else(|| mlua::Error::runtime("vim.uv callback context is unavailable"))?;
                    let scheduler = context.scheduler.clone();
                    let fast = context.fast.clone();
                    drop(context);
                    this.timer
                        .close_with(&mut this.uv_loop.borrow_mut(), move |_, _| {
                            schedule_callback(
                                scheduler.clone(),
                                lua.clone(),
                                callback.clone(),
                                MultiValue::new(),
                                fast.clone(),
                            )
                            .map_err(CallbackError::new)
                        })
                        .map_err(mlua::Error::external)?;
                }
                None => this.timer.close(&mut this.uv_loop.borrow_mut()).map_err(mlua::Error::external)?,
            }
            Ok(())
        });
    }
}

#[derive(Clone)]
struct CallbackContext {
    scheduler: Rc<dyn Scheduler>,
    fast: FastCallbackState,
}

pub(crate) fn install(
    lua: &Lua,
    scheduler: Rc<dyn Scheduler>,
    fast: FastCallbackState,
    _runtime_root: RuntimeRoot,
    _builtins: Rc<dyn BuiltinHost>,
) -> mlua::Result<()> {
    lua.set_app_data(CallbackContext { scheduler: scheduler.clone(), fast: fast.clone() });

    let uv_loop = Rc::new(RefCell::new(UvLoop::new().map_err(mlua::Error::external)?));
    let state = Rc::new(CoreState::new());
    let vim: Table = lua.globals().get("vim")?;
    let uv = lua.create_table()?;

    let loop_for_run = uv_loop.clone();
    uv.set("run", lua.create_function(move |_, mode: Option<String>| {
        let mode = match mode.as_deref().unwrap_or("default") {
            "default" => RunMode::Default,
            "once" => RunMode::Once,
            "nowait" => RunMode::NoWait,
            other => return Err(mlua::Error::runtime(format!("invalid run mode: {other}"))),
        };
        loop_for_run.borrow_mut().run(mode).map_err(mlua::Error::external)
    })?)?;

    let loop_for_stop = uv_loop.clone();
    uv.set("stop", lua.create_function(move |_, ()| {
        loop_for_stop.borrow_mut().stop();
        Ok(())
    })?)?;

    let loop_for_alive = uv_loop.clone();
    uv.set("loop_alive", lua.create_function(move |_, ()| Ok(loop_for_alive.borrow().loop_alive()))?)?;

    let loop_for_now = uv_loop.clone();
    uv.set("now", lua.create_function(move |_, ()| Ok(loop_for_now.borrow().now()))?)?;

    let loop_for_update = uv_loop.clone();
    uv.set("update_time", lua.create_function(move |_, ()| {
        loop_for_update.borrow_mut().update_time();
        Ok(())
    })?)?;

    uv.set("hrtime", lua.create_function(|_, ()| {
        static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
        let nanos = ORIGIN.elapsed().as_nanos();
        Ok(u64::try_from(nanos).unwrap_or(u64::MAX))
    })?)?;

    let loop_for_timer = uv_loop.clone();
    uv.set("new_timer", lua.create_function(move |lua, ()| {
        let timer = Timer::new(&mut loop_for_timer.borrow_mut()).map_err(mlua::Error::external)?;
        lua.create_userdata(LuaTimer { timer, uv_loop: loop_for_timer.clone() })
    })?)?;

    install_files(
        lua,
        &uv,
        state,
        scheduler.clone(),
        fast.clone(),
    )?;
    crate::uv_handles::install(lua, &uv, uv_loop, scheduler, fast)?;
    vim.set("uv", uv.clone())?;
    vim.set("loop", uv)?;
    Ok(())
}

fn install_files(
    lua: &Lua,
    uv: &Table,
    state: Rc<CoreState>,
    scheduler: Rc<dyn Scheduler>,
    fast: FastCallbackState,
) -> mlua::Result<()> {
    let open_state = state.clone();
    let open_scheduler = scheduler.clone();
    let open_fast = fast.clone();
    uv.set("fs_open", lua.create_function(move |lua, (path, flags, _mode, callback): (String, String, Option<u32>, Option<Function>)| {
        let result = open_file(&path, &flags).map(|file| open_state.insert_file(file));
        finish_fs(lua, result, callback, open_scheduler.clone(), open_fast.clone())
    })?)?;

    let read_state = state.clone();
    let read_scheduler = scheduler.clone();
    let read_fast = fast.clone();
    uv.set("fs_read", lua.create_function(move |lua, (descriptor, length, offset, callback): (i64, usize, Option<u64>, Option<Function>)| {
        let result = (|| {
            let mut files = read_state.files.borrow_mut();
            let file = files.get_mut(&descriptor).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "bad file descriptor"))?;
            if let Some(offset) = offset { file.seek(SeekFrom::Start(offset))?; }
            let mut bytes = vec![0; length];
            let count = file.read(&mut bytes)?;
            bytes.truncate(count);
            Ok(bytes)
        })();
        let result = match result {
            Ok(bytes) => Ok(lua.create_string(bytes)?),
            Err(error) => Err(error),
        };
        finish_fs(lua, result, callback, read_scheduler.clone(), read_fast.clone())
    })?)?;

    let write_state = state.clone();
    let write_scheduler = scheduler.clone();
    let write_fast = fast.clone();
    uv.set("fs_write", lua.create_function(move |lua, (descriptor, bytes, offset, callback): (i64, mlua::LuaString, Option<u64>, Option<Function>)| {
        let result = (|| {
            let mut files = write_state.files.borrow_mut();
            let file = files.get_mut(&descriptor).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "bad file descriptor"))?;
            if let Some(offset) = offset { file.seek(SeekFrom::Start(offset))?; }
            file.write(bytes.as_bytes().as_ref())
        })();
        finish_fs(lua, result, callback, write_scheduler.clone(), write_fast.clone())
    })?)?;

    let close_state = state;
    uv.set("fs_close", lua.create_function(move |lua, (descriptor, callback): (i64, Option<Function>)| {
        let result = close_state.files.borrow_mut().remove(&descriptor)
            .map(|_| true)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "bad file descriptor"));
        finish_fs(lua, result, callback, scheduler.clone(), fast.clone())
    })?)?;
    Ok(())
}

fn open_file(path: &str, flags: &str) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    match flags {
        "r" => { options.read(true); }
        "r+" => { options.read(true).write(true); }
        "w" => { options.write(true).create(true).truncate(true); }
        "w+" => { options.read(true).write(true).create(true).truncate(true); }
        "a" => { options.write(true).create(true).append(true); }
        "a+" => { options.read(true).write(true).create(true).append(true); }
        _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid open flags")),
    }
    options.open(path)
}

fn finish_fs<T: mlua::IntoLuaMulti + 'static>(
    lua: &Lua,
    result: std::io::Result<T>,
    callback: Option<Function>,
    scheduler: Rc<dyn Scheduler>,
    fast: FastCallbackState,
) -> mlua::Result<MultiValue> {
    match (result, callback) {
        (Ok(value), Some(callback)) => {
            let mut args = value.into_lua_multi(lua)?;
            args.push_front(Value::Nil);
            schedule_callback(scheduler, lua.clone(), callback, args, fast).map_err(mlua::Error::runtime)?;
            Ok(MultiValue::new())
        }
        (Err(error), Some(callback)) => {
            let args = error_values(lua, &error)?;
            schedule_callback(scheduler, lua.clone(), callback, args, fast).map_err(mlua::Error::runtime)?;
            Ok(MultiValue::new())
        }
        (Ok(value), None) => value.into_lua_multi(lua),
        (Err(error), None) => error_values(lua, &error),
    }
}

fn error_values(lua: &Lua, error: &std::io::Error) -> mlua::Result<MultiValue> {
    let name = match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        std::io::ErrorKind::InvalidInput => "EINVAL",
        std::io::ErrorKind::WouldBlock => "EAGAIN",
        _ => "EIO",
    };
    Ok(MultiValue::from_vec(vec![
        Value::Nil,
        Value::String(lua.create_string(error.to_string())?),
        Value::String(lua.create_string(name)?),
    ]))
}

fn schedule_callback(
    scheduler: Rc<dyn Scheduler>,
    lua: Lua,
    callback: Function,
    args: MultiValue,
    fast: FastCallbackState,
) -> Result<(), String> {
    scheduler.schedule_deferred(Box::new(move || {
        let _guard = fast.enter();
        call_with_traceback(&lua, &callback, args).map(|_| ())
    }))
}
