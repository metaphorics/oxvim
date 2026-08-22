//! Behavior-critical `vim.uv` bindings shared by the complete UV adapter.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use mlua::{
    AnyUserData, FromLuaMulti, Function, IntoLua, Lua, LuaString, MultiValue, Table, UserData,
    UserDataMethods, Value,
};
use ox_uv::fs::{
    self as uvfs, DirEntryType, FileHandle, FsError, FsResult, FsTime, OpenFlags, Stat, StatFs,
};
use ox_uv::misc;
use ox_uv::{CallbackError, Handle, RunMode, Timer, UvLoop};

use crate::host::RuntimeRoot;
use crate::uv_handles::LoopAccess;
use crate::vim::{call_with_traceback, BuiltinHost, FastCallbackState, Scheduler};

struct CoreState {
    files: RefCell<HashMap<i64, FileHandle>>,
    next_file: RefCell<i64>,
}

impl CoreState {
    fn new() -> Self {
        Self { files: RefCell::new(HashMap::new()), next_file: RefCell::new(3) }
    }

    fn insert_file(&self, file: FileHandle) -> i64 {
        let mut next = self.next_file.borrow_mut();
        let descriptor = *next;
        *next = next.saturating_add(1);
        self.files.borrow_mut().insert(descriptor, file);
        descriptor
    }

    /// Looks up a live descriptor, sharing ownership of the underlying file.
    fn file(&self, descriptor: i64) -> FsResult<FileHandle> {
        self.files.borrow().get(&descriptor).cloned().ok_or_else(bad_file_descriptor)
    }
}

#[derive(Clone)]
struct LuaTimer {
    timer: Timer,
    access: LoopAccess,
    closing: Rc<Cell<bool>>,
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
                let timer = this.timer;
                let access = this.access.clone();
                let event_access = access.clone();
                access.apply(Box::new(move |uv_loop| {
                    let _ = timer.start(uv_loop, timeout, repeat, move |loop_, _| {
                        event_access
                            .callback(loop_, || {
                                schedule_callback(
                                    &scheduler,
                                    &lua,
                                    callback.clone(),
                                    MultiValue::new(),
                                    &fast,
                                )
                            })
                            .map_err(CallbackError::new)
                    });
                }))?;
                Ok(true)
            },
        );
        methods.add_method("stop", |_, this, ()| {
            let timer = this.timer;
            this.access.apply(Box::new(move |uv_loop| { let _ = timer.stop(uv_loop); }))?;
            Ok(true)
        });
        methods.add_method("again", |_, this, ()| {
            let timer = this.timer;
            this.access.apply(Box::new(move |uv_loop| { let _ = timer.again(uv_loop); }))?;
            Ok(true)
        });
        methods.add_method("set_repeat", |_, this, repeat: u64| {
            let timer = this.timer;
            this.access.apply(Box::new(move |uv_loop| { let _ = timer.set_repeat(uv_loop, repeat); }))?;
            Ok(())
        });
        methods.add_method("get_repeat", |_, this, ()| {
            let uv_loop = this.access.uv_loop.try_borrow().map_err(|_| mlua::Error::runtime("timer repeat is unavailable during its callback"))?;
            this.timer.get_repeat(&uv_loop).map_err(mlua::Error::external)
        });
        methods.add_method("ref", |_, this, ()| {
            let timer = this.timer;
            this.access.apply(Box::new(move |uv_loop| { let _ = timer.ref_(uv_loop); }))?;
            Ok(this.clone())
        });
        methods.add_method("unref", |_, this, ()| {
            let timer = this.timer;
            this.access.apply(Box::new(move |uv_loop| { let _ = timer.unref(uv_loop); }))?;
            Ok(this.clone())
        });
        methods.add_method("has_ref", |_, this, ()| {
            Ok(this.access.uv_loop.try_borrow().map_or(true, |uv_loop| this.timer.has_ref(&uv_loop)))
        });
        methods.add_method("is_active", |_, this, ()| {
            Ok(this.access.uv_loop.try_borrow().map_or(!this.closing.get(), |uv_loop| this.timer.is_active(&uv_loop)))
        });
        methods.add_method("is_closing", |_, this, ()| {
            Ok(this.closing.get() || this.access.uv_loop.try_borrow().is_ok_and(|uv_loop| this.timer.is_closing(&uv_loop)))
        });
        methods.add_method("close", |lua, this, callback: Option<Function>| {
            if this.closing.replace(true) {
                return Ok(());
            }
            let timer = this.timer;
            match callback {
                Some(callback) => {
                    let lua = lua.clone();
                    let context = lua
                        .app_data_ref::<CallbackContext>()
                        .ok_or_else(|| mlua::Error::runtime("vim.uv callback context is unavailable"))?;
                    let scheduler = context.scheduler.clone();
                    let fast = context.fast.clone();
                    drop(context);
                    this.access.apply(Box::new(move |uv_loop| {
                        let _ = timer.close_with(uv_loop, move |_, _| {
                            schedule_callback(&scheduler, &lua, callback.clone(), MultiValue::new(), &fast)
                                .map_err(CallbackError::new)
                        });
                    }))?;
                }
                None => this.access.apply(Box::new(move |uv_loop| { let _ = timer.close(uv_loop); }))?,
            }
            Ok(())
        });
    }
}

/// One `uv.fs_scandir()` request: a synchronous cursor over sorted entries.
struct LuaScandir {
    entries: RefCell<uvfs::Scandir>,
}

impl UserData for LuaScandir {}

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
    let core: Table = vim.get("_core")?;
    core.set("ui_flush", lua.create_function(|_, ()| Ok(()))?)?;
    core.set("check_interrupt", lua.create_function(|_, ()| Ok(false))?)?;

    let loop_for_poll = uv_loop.clone();
    core.set("loop_poll", lua.create_function(move |_, (timeout, _fast_only): (i64, bool)| {
        let mut uv_loop = loop_for_poll.borrow_mut();
        let timeout_timer = if timeout >= 0 {
            let timer = Timer::new(&mut uv_loop).map_err(mlua::Error::external)?;
            timer
                .start(&mut uv_loop, timeout as u64, 0, |_, _| Ok(()))
                .map_err(mlua::Error::external)?;
            Some(timer)
        } else {
            None
        };
        uv_loop.run(RunMode::Once).map_err(mlua::Error::external)?;
        if let Some(timer) = timeout_timer {
            timer.close(&mut uv_loop).map_err(mlua::Error::external)?;
            uv_loop.run(RunMode::NoWait).map_err(mlua::Error::external)?;
        }
        Ok(())
    })?)?;

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

    let loop_for_timer = uv_loop.clone();
    let timer_access = LoopAccess::new(uv_loop.clone());
    uv.set("new_timer", lua.create_function(move |lua, ()| {
        let timer = Timer::new(&mut loop_for_timer.borrow_mut()).map_err(mlua::Error::external)?;
        lua.create_userdata(LuaTimer {
            timer,
            access: timer_access.clone(),
            closing: Rc::new(Cell::new(false)),
        })
    })?)?;

    install_misc(lua, &uv)?;
    install_fs(lua, &uv, &state, &scheduler, &fast)?;
    crate::uv_handles::install(lua, &uv, uv_loop, scheduler, fast)?;
    vim.set("uv", uv.clone())?;
    vim.set("loop", uv)?;
    Ok(())
}

/// Miscellaneous utilities: `luv-miscellaneous-utilities` in luvref.txt.
///
/// These calls have no async form; failures use the luv `fail` return
/// `nil, err, name`.
fn install_misc(lua: &Lua, uv: &Table) -> mlua::Result<()> {
    uv.set("hrtime", lua.create_function(|_, ()| Ok(lua_int(misc::hrtime())))?)?;

    uv.set("cwd", lua.create_function(|lua, ()| match misc::cwd() {
        Ok(path) => single_value(lua, path.to_string_lossy().into_owned()),
        Err(error) => uv_fail(lua, &error),
    })?)?;

    uv.set("chdir", lua.create_function(|lua, (directory,): (String,)| match misc::chdir(&directory) {
        Ok(()) => single_value(lua, 0_i64),
        Err(error) => uv_fail(lua, &error),
    })?)?;

    uv.set("os_homedir", lua.create_function(|lua, ()| match misc::os_homedir() {
        Ok(path) => single_value(lua, path.to_string_lossy().into_owned()),
        Err(error) => uv_fail(lua, &error),
    })?)?;

    uv.set("os_tmpdir", lua.create_function(|_, ()| {
        Ok(misc::os_tmpdir().to_string_lossy().into_owned())
    })?)?;

    uv.set("os_uname", lua.create_function(|lua, ()| {
        let uname = misc::os_uname();
        let table = lua.create_table()?;
        table.set("sysname", uname.sysname.as_str())?;
        table.set("release", uname.release.as_str())?;
        table.set("version", uname.version.as_str())?;
        table.set("machine", uname.machine.as_str())?;
        Ok(table)
    })?)?;

    uv.set("getpid", lua.create_function(|_, ()| Ok(misc::getpid()))?)?;
    uv.set("os_getpid", lua.create_function(|_, ()| Ok(misc::getpid()))?)?;

    uv.set("gettimeofday", lua.create_function(|lua, ()| match misc::gettimeofday() {
        Ok((seconds, microseconds)) => {
            let mut values = MultiValue::new();
            values.push_back(Value::Integer(lua_int(seconds)));
            values.push_back(Value::Integer(i64::from(microseconds)));
            Ok(values)
        }
        Err(error) => uv_fail(lua, &error),
    })?)?;

    uv.set("exepath", lua.create_function(|lua, ()| match misc::exepath() {
        Ok(path) => single_value(lua, path.to_string_lossy().into_owned()),
        Err(error) => uv_fail(lua, &error),
    })?)?;

    uv.set("uptime", lua.create_function(|lua, ()| match misc::uptime() {
        Ok(seconds) => single_value(lua, seconds),
        Err(error) => uv_fail(lua, &error),
    })?)?;

    uv.set("loadavg", lua.create_function(|_, ()| {
        let (one, five, fifteen) = misc::loadavg();
        Ok((one, five, fifteen))
    })?)?;

    uv.set("get_total_memory", lua.create_function(|_, ()| Ok(lua_int(misc::get_total_memory())))?)?;
    uv.set("get_free_memory", lua.create_function(|_, ()| Ok(lua_int(misc::get_free_memory())))?)?;

    uv.set("os_getenv", lua.create_function(|lua, (name,): (String,)| match misc::os_getenv(&name) {
        Some(value) => single_value(lua, value.to_string_lossy().into_owned()),
        None => uv_fail(
            lua,
            &ox_uv::Error::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "environment variable is not set",
            )),
        ),
    })?)?;
    uv.set("os_environ", lua.create_function(|lua, ()| {
        let environment = lua.create_table()?;
        for (name, value) in std::env::vars_os() {
            environment.set(
                name.to_string_lossy().as_ref(),
                value.to_string_lossy().as_ref(),
            )?;
        }
        Ok(environment)
    })?)?;
    Ok(())
}

/// Filesystem operations: `luv-file-system-operations` in luvref.txt.
///
/// Without a trailing callback an operation runs synchronously and returns
/// its results (or the `fail` shape `nil, err, name`); with a callback the
/// operation still runs synchronously and the callback is scheduled on the
/// editor loop with `nil` (or the error string) as its first argument.
fn install_fs(
    lua: &Lua,
    uv: &Table,
    state: &Rc<CoreState>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
) -> mlua::Result<()> {
    install_fs_file_ops(lua, uv, state, scheduler, fast)?;
    install_fs_attribute_ops(lua, uv, state, scheduler, fast)?;
    install_fs_path_ops(lua, uv, state, scheduler, fast)?;
    install_fs_stat_ops(lua, uv, state, scheduler, fast)?;

    // `fs_scandir` always returns its request handle synchronously, callback
    // or not, and passes the same handle to the callback.
    let scandir_scheduler = Rc::clone(scheduler);
    let scandir_fast = fast.clone();
    uv.set(
        "fs_scandir",
        lua.create_function(move |lua, (path, callback): (String, Option<Function>)| {
            match uvfs::scandir(&path) {
                Ok(scan) => {
                    let handle = lua.create_userdata(LuaScandir { entries: RefCell::new(scan) })?;
                    if let Some(callback) = callback {
                        let mut args = MultiValue::new();
                        args.push_back(Value::Nil);
                        args.push_back(Value::UserData(handle.clone()));
                        schedule_callback(
                            &scandir_scheduler,
                            lua,
                            callback,
                            args,
                            &scandir_fast,
                        )
                        .map_err(mlua::Error::runtime)?;
                    }
                    let mut values = MultiValue::new();
                    values.push_back(Value::UserData(handle));
                    Ok(values)
                }
                Err(error) => {
                    if let Some(callback) = callback {
                        let args = MultiValue::from_vec(vec![
                            Value::String(lua.create_string(error.to_string())?),
                        ]);
                        schedule_callback(
                            &scandir_scheduler,
                            lua,
                            callback,
                            args,
                            &scandir_fast,
                        )
                        .map_err(mlua::Error::runtime)?;
                    }
                    fs_fail(lua, &error)
                }
            }
        })?,
    )?;

    uv.set(
        "fs_scandir_next",
        lua.create_function(|lua, scan: AnyUserData| {
            let scan = scan.borrow::<LuaScandir>()?;
            let entry = scan.entries.borrow_mut().next();
            match entry {
                Some(entry) => {
                    let mut values = MultiValue::new();
                    values.push_back(Value::String(lua.create_string(entry.name)?));
                    values
                        .push_back(Value::String(lua.create_string(entry_type_name(entry.kind))?));
                    Ok(values)
                }
                None => Ok(MultiValue::new()),
            }
        })?,
    )?;
    Ok(())
}

/// Descriptor-based operations: open/close/read/write plus fsync-family,
/// sendfile, and mkstemp (which produces a new descriptor).
fn install_fs_file_ops(
    lua: &Lua,
    uv: &Table,
    state: &Rc<CoreState>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
) -> mlua::Result<()> {
    register_fs_op(lua, uv, "fs_open", state, scheduler, fast, |lua, state, args| {
        let (path, flags, mode, callback) =
            <(String, Value, Option<u32>, Option<Function>)>::from_lua_multi(args, lua)?;
        let flags = parse_open_flags(flags)?;
        let result =
            uvfs::open(&path, flags, mode.unwrap_or(0)).map(|handle| state.insert_file(handle));
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_close", state, scheduler, fast, |lua, state, args| {
        let (descriptor, callback) = <(i64, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = match state.files.borrow_mut().remove(&descriptor) {
            Some(handle) => uvfs::close(&handle).map(|()| true),
            None => Err(bad_file_descriptor()),
        };
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_read", state, scheduler, fast, |lua, state, args| {
        let (descriptor, size, offset, callback) =
            <(i64, usize, Option<i64>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = state
            .file(descriptor)
            .and_then(|handle| uvfs::read(&handle, size, file_offset(offset)))
            .and_then(|bytes| lua.create_string(bytes).map_err(|error| lua_error(&error)));
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_write", state, scheduler, fast, |lua, state, args| {
        let (descriptor, data, offset, callback) =
            <(i64, LuaString, Option<i64>, Option<Function>)>::from_lua_multi(args, lua)?;
        let bytes = data.as_bytes().to_vec();
        let result = state
            .file(descriptor)
            .and_then(|handle| uvfs::write(&handle, &bytes, file_offset(offset)))
            .map(|count| lua_int(u64::try_from(count).unwrap_or(u64::MAX)));
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_fstat", state, scheduler, fast, |lua, state, args| {
        let (descriptor, callback) = <(i64, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = state
            .file(descriptor)
            .and_then(|handle| uvfs::fstat(&handle))
            .and_then(|stat| stat_table(lua, &stat).map_err(|error| lua_error(&error)));
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_fsync", state, scheduler, fast, |lua, state, args| {
        let (descriptor, callback) = <(i64, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = state.file(descriptor).and_then(|handle| uvfs::fsync(&handle)).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_fdatasync", state, scheduler, fast, |lua, state, args| {
        let (descriptor, callback) = <(i64, Option<Function>)>::from_lua_multi(args, lua)?;
        let result =
            state.file(descriptor).and_then(|handle| uvfs::fdatasync(&handle)).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_sendfile", state, scheduler, fast, |lua, state, args| {
        let (out_descriptor, in_descriptor, in_offset, size, callback) =
            <(i64, i64, Option<i64>, Option<usize>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = (|| {
            let out_handle = state.file(out_descriptor)?;
            let in_handle = state.file(in_descriptor)?;
            let offset = match in_offset {
                Some(offset) if offset >= 0 => u64::try_from(offset).unwrap_or(u64::MAX),
                _ => 0,
            };
            uvfs::sendfile(&out_handle, &in_handle, offset, size.unwrap_or(0))
                .map(|written| lua_int(u64::try_from(written).unwrap_or(u64::MAX)))
        })();
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_mkstemp", state, scheduler, fast, |lua, state, args| {
        let (template, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::mkstemp(&template).map(|(handle, path)| {
            (state.insert_file(handle), path.to_string_lossy().into_owned())
        });
        Ok((result, callback))
    })?;
    Ok(())
}

/// Attribute operations: permission bits, access checks, truncation, and the
/// utime family.
fn install_fs_attribute_ops(
    lua: &Lua,
    uv: &Table,
    state: &Rc<CoreState>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
) -> mlua::Result<()> {
    register_fs_op(lua, uv, "fs_chmod", state, scheduler, fast, |lua, _state, args| {
        let (path, mode, callback) =
            <(String, Option<u32>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::chmod(&path, mode.unwrap_or(0)).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_fchmod", state, scheduler, fast, |lua, state, args| {
        let (descriptor, mode, callback) =
            <(i64, Option<u32>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = state
            .file(descriptor)
            .and_then(|handle| uvfs::fchmod(&handle, mode.unwrap_or(0)))
            .map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_access", state, scheduler, fast, |lua, _state, args| {
        let (path, mode, callback) =
            <(String, Value, Option<Function>)>::from_lua_multi(args, lua)?;
        let (read, write, execute) = access_mode(mode).map_err(mlua::Error::external)?;
        let result = uvfs::access(&path, read, write, execute);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_truncate", state, scheduler, fast, |lua, _state, args| {
        let (path, length, callback) =
            <(String, Option<u64>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::truncate(&path, length.unwrap_or(0)).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_ftruncate", state, scheduler, fast, |lua, state, args| {
        let (descriptor, length, callback) =
            <(i64, Option<u64>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = state
            .file(descriptor)
            .and_then(|handle| uvfs::ftruncate(&handle, length.unwrap_or(0)))
            .map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_utime", state, scheduler, fast, |lua, _state, args| {
        let (path, atime, mtime, callback) =
            <(String, Option<Value>, Option<Value>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = (|| {
            let current = uvfs::stat(&path)?;
            let atime = resolve_utime(atime, current.atime)?;
            let mtime = resolve_utime(mtime, current.mtime)?;
            uvfs::utime(&path, atime, mtime).map(|()| true)
        })();
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_lutime", state, scheduler, fast, |lua, _state, args| {
        let (path, atime, mtime, callback) =
            <(String, Option<Value>, Option<Value>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = (|| {
            let current = uvfs::lstat(&path)?;
            let atime = resolve_utime(atime, current.atime)?;
            let mtime = resolve_utime(mtime, current.mtime)?;
            uvfs::lutime(&path, atime, mtime).map(|()| true)
        })();
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_futime", state, scheduler, fast, |lua, state, args| {
        let (descriptor, atime, mtime, callback) =
            <(i64, Option<Value>, Option<Value>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = (|| {
            let handle = state.file(descriptor)?;
            let current = uvfs::fstat(&handle)?;
            let atime = resolve_utime(atime, current.atime)?;
            let mtime = resolve_utime(mtime, current.mtime)?;
            uvfs::futime(&handle, atime, mtime).map(|()| true)
        })();
        Ok((result, callback))
    })?;
    Ok(())
}

/// Path-shape operations: directory and link management, copyfile, mkdtemp.
fn install_fs_path_ops(
    lua: &Lua,
    uv: &Table,
    state: &Rc<CoreState>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
) -> mlua::Result<()> {
    register_fs_op(lua, uv, "fs_mkdir", state, scheduler, fast, |lua, _state, args| {
        let (path, mode, callback) =
            <(String, Option<u32>, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::mkdir(&path, mode.unwrap_or(0o777)).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_rmdir", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::rmdir(&path).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_rename", state, scheduler, fast, |lua, _state, args| {
        let (from, to, callback) = <(String, String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::rename(&from, &to).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_unlink", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::unlink(&path).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_link", state, scheduler, fast, |lua, _state, args| {
        let (from, to, callback) = <(String, String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::link(&from, &to).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_symlink", state, scheduler, fast, |lua, _state, args| {
        let (target, link_path, third, fourth) =
            <(String, String, Option<Value>, Option<Function>)>::from_lua_multi(args, lua)?;
        // Without a flags table the third parameter is the callback.
        let (flags, callback) = match third {
            Some(Value::Function(callback)) => (None, Some(callback)),
            flags => (flags, fourth),
        };
        let directory = symlink_directory(flags).map_err(mlua::Error::external)?;
        let result = uvfs::symlink(&target, &link_path, directory).map(|()| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_readlink", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::readlink(&path).map(|path| path.to_string_lossy().into_owned());
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_copyfile", state, scheduler, fast, |lua, _state, args| {
        let (from, to, third, fourth) =
            <(String, String, Option<Value>, Option<Function>)>::from_lua_multi(args, lua)?;
        // Without a flags table the third parameter is the callback.
        let (flags, callback) = match third {
            Some(Value::Function(callback)) => (None, Some(callback)),
            flags => (flags, fourth),
        };
        let exclusive = copyfile_exclusive(flags).map_err(mlua::Error::external)?;
        let result = uvfs::copyfile(&from, &to, exclusive).map(|_copied| true);
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_mkdtemp", state, scheduler, fast, |lua, _state, args| {
        let (template, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::mkdtemp(&template).map(|path| path.to_string_lossy().into_owned());
        Ok((result, callback))
    })?;
    Ok(())
}

/// Read-only metadata operations.
fn install_fs_stat_ops(
    lua: &Lua,
    uv: &Table,
    state: &Rc<CoreState>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
) -> mlua::Result<()> {
    register_fs_op(lua, uv, "fs_stat", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result =
            uvfs::stat(&path).and_then(|stat| stat_table(lua, &stat).map_err(|error| lua_error(&error)));
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_lstat", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result =
            uvfs::lstat(&path).and_then(|stat| stat_table(lua, &stat).map_err(|error| lua_error(&error)));
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_realpath", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result = uvfs::realpath(&path).map(|path| path.to_string_lossy().into_owned());
        Ok((result, callback))
    })?;

    register_fs_op(lua, uv, "fs_statfs", state, scheduler, fast, |lua, _state, args| {
        let (path, callback) = <(String, Option<Function>)>::from_lua_multi(args, lua)?;
        let result =
            uvfs::statfs(&path).and_then(|stats| statfs_table(lua, &stats).map_err(|error| lua_error(&error)));
        Ok((result, callback))
    })?;
    Ok(())
}

/// Registers one filesystem operation under `name`.
///
/// The operation parses its arguments (including a trailing callback) from
/// the raw [`MultiValue`] and returns its result plus the callback, if any.
fn register_fs_op<T, F>(
    lua: &Lua,
    uv: &Table,
    name: &str,
    state: &Rc<CoreState>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
    operation: F,
) -> mlua::Result<()>
where
    T: mlua::IntoLuaMulti + 'static,
    F: Fn(&Lua, &CoreState, MultiValue) -> mlua::Result<(FsResult<T>, Option<Function>)> + 'static,
{
    let state = Rc::clone(state);
    let scheduler = Rc::clone(scheduler);
    let fast = fast.clone();
    uv.set(
        name,
        lua.create_function(move |lua, args: MultiValue| {
            let (result, callback) = operation(lua, &state, args)?;
            finish_fs(lua, result, callback, &scheduler, &fast)
        })?,
    )
}

fn finish_fs<T: mlua::IntoLuaMulti + 'static>(
    lua: &Lua,
    result: FsResult<T>,
    callback: Option<Function>,
    scheduler: &Rc<dyn Scheduler>,
    fast: &FastCallbackState,
) -> mlua::Result<MultiValue> {
    match (result, callback) {
        (Ok(value), Some(callback)) => {
            let mut args = value.into_lua_multi(lua)?;
            args.push_front(Value::Nil);
            schedule_callback(scheduler, lua, callback, args, fast)
                .map_err(mlua::Error::runtime)?;
            Ok(MultiValue::new())
        }
        (Err(error), Some(callback)) => {
            let args = MultiValue::from_vec(vec![
                Value::String(lua.create_string(error.to_string())?),
            ]);
            schedule_callback(scheduler, lua, callback, args, fast)
                .map_err(mlua::Error::runtime)?;
            Ok(MultiValue::new())
        }
        (Ok(value), None) => value.into_lua_multi(lua),
        (Err(error), None) => fs_fail(lua, &error),
    }
}

/// Synchronous failure shape: `nil, err, name` (luv `fail` returns).
fn fs_fail(lua: &Lua, error: &FsError) -> mlua::Result<MultiValue> {
    Ok(MultiValue::from_vec(vec![
        Value::Nil,
        Value::String(lua.create_string(error.message.as_str())?),
        Value::String(lua.create_string(error.name)?),
    ]))
}

/// Synchronous failure shape for loop-level errors.
fn uv_fail(lua: &Lua, error: &ox_uv::Error) -> mlua::Result<MultiValue> {
    let (name, message) = uv_error_parts(error);
    Ok(MultiValue::from_vec(vec![
        Value::Nil,
        Value::String(lua.create_string(message)?),
        Value::String(lua.create_string(name)?),
    ]))
}

fn uv_error_parts(error: &ox_uv::Error) -> (&'static str, String) {
    match error {
        ox_uv::Error::Io(inner) => (errno_name(inner), inner.to_string()),
        ox_uv::Error::MissingEnvironment(name) => {
            ("ENOENT", format!("environment variable {name} is not set"))
        }
        ox_uv::Error::Unsupported { .. } => ("ENOTSUP", error.to_string()),
        _ => ("EINVAL", error.to_string()),
    }
}

fn errno_name(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::InvalidInput => "EINVAL",
        io::ErrorKind::WouldBlock => "EAGAIN",
        _ => "EIO",
    }
}

fn bad_file_descriptor() -> FsError {
    FsError { name: "EBADF", message: "bad file descriptor".into(), raw_os_error: None }
}

fn lua_error(error: &mlua::Error) -> FsError {
    FsError { name: "EINVAL", message: error.to_string(), raw_os_error: None }
}

/// Converts an `fs_read`/`fs_write` offset: `nil` or negative means "use the
/// current file offset", as in luv.
fn file_offset(offset: Option<i64>) -> Option<u64> {
    offset.filter(|value| *value >= 0).and_then(|value| u64::try_from(value).ok())
}

fn lua_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn single_value(lua: &Lua, value: impl IntoLua) -> mlua::Result<MultiValue> {
    let mut values = MultiValue::new();
    values.push_back(value.into_lua(lua)?);
    Ok(values)
}

fn parse_open_flags(value: Value) -> mlua::Result<OpenFlags> {
    match value {
        Value::String(text) => {
            open_flags_from_str(&text.to_string_lossy()).map_err(mlua::Error::external)
        }
        Value::Integer(bits) => open_flags_from_bits(bits).map_err(mlua::Error::external),
        other => Err(mlua::Error::runtime(format!(
            "open flags must be a string or integer, got {}",
            other.type_name()
        ))),
    }
}

/// Parses the string flag set documented by `uv.fs_open()`.
///
/// The `'s'`/`'rs'`-style synchronous-I/O suffixes are accepted but have no
/// portable expression in `std::fs::OpenOptions` and are ignored.
fn open_flags_from_str(flags: &str) -> FsResult<OpenFlags> {
    let exclusive = flags.contains('x');
    let core: String = flags.chars().filter(|c| !matches!(c, 'x' | 's')).collect();
    let mut parsed = match core.as_str() {
        "r" => OpenFlags::READ,
        "r+" => OpenFlags::READ_WRITE,
        "w" => OpenFlags::WRITE,
        "w+" => OpenFlags { read: true, ..OpenFlags::WRITE },
        "a" => OpenFlags { truncate: false, append: true, ..OpenFlags::WRITE },
        "a+" => OpenFlags { read: true, truncate: false, append: true, ..OpenFlags::WRITE },
        _ => {
            return Err(FsError {
                name: "EINVAL",
                message: format!("invalid open flags: {flags}"),
                raw_os_error: None,
            })
        }
    };
    if exclusive {
        parsed.create_new = true;
        parsed.create = true;
        parsed.truncate = false;
    }
    Ok(parsed)
}

/// Parses integer `O_*` bits on Linux, where `uv.fs_open()` documents them.
fn open_flags_from_bits(bits: i64) -> FsResult<OpenFlags> {
    #[cfg(target_os = "linux")]
    {
        const O_CREAT: i64 = 0o100;
        const O_EXCL: i64 = 0o200;
        const O_TRUNC: i64 = 0o1000;
        const O_APPEND: i64 = 0o2000;
        if bits < 0 {
            return Err(FsError {
                name: "EINVAL",
                message: "open flag bits must be non-negative".into(),
                raw_os_error: None,
            });
        }
        let access = bits & 0o3;
        Ok(OpenFlags {
            read: access != 1,
            write: access == 1 || access == 2,
            append: bits & O_APPEND != 0,
            truncate: bits & O_TRUNC != 0,
            create: bits & O_CREAT != 0,
            create_new: bits & O_EXCL != 0,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = bits;
        Err(FsError {
            name: "EINVAL",
            message: "integer open flags are Linux-only; pass a flag string".into(),
            raw_os_error: None,
        })
    }
}

/// Parses an `fs_access` mode: an `'r'`/`'w'`/`'x'` string or the `access(2)`
/// integer bits.
fn access_mode(mode: Value) -> FsResult<(bool, bool, bool)> {
    match mode {
        Value::String(text) => {
            let mut read = false;
            let mut write = false;
            let mut execute = false;
            for character in text.to_str().map_err(|error| lua_error(&error))?.chars() {
                match character {
                    'r' | 'R' => read = true,
                    'w' | 'W' => write = true,
                    'x' | 'X' => execute = true,
                    other => {
                        return Err(FsError {
                            name: "EINVAL",
                            message: format!("invalid access mode character: {other}"),
                            raw_os_error: None,
                        })
                    }
                }
            }
            Ok((read, write, execute))
        }
        Value::Integer(bits) => Ok((bits & 4 != 0, bits & 2 != 0, bits & 1 != 0)),
        other => Err(FsError {
            name: "EINVAL",
            message: format!("invalid access mode: {}", other.type_name()),
            raw_os_error: None,
        }),
    }
}

/// Parses `fs_symlink` flags: `{ dir = boolean }` or `UV_FS_SYMLINK_DIR`.
fn symlink_directory(flags: Option<Value>) -> FsResult<bool> {
    match flags {
        None | Some(Value::Nil) => Ok(false),
        Some(Value::Integer(bits)) => Ok(bits & 1 != 0),
        Some(Value::Table(table)) => {
            table.get::<Option<bool>>("dir").map_err(|error| lua_error(&error)).map(|dir| dir.unwrap_or(false))
        }
        Some(other) => Err(FsError {
            name: "EINVAL",
            message: format!("invalid symlink flags: {}", other.type_name()),
            raw_os_error: None,
        }),
    }
}

/// Parses `fs_copyfile` flags: `{ excl = boolean }` or `UV_FS_COPYFILE_EXCL`.
fn copyfile_exclusive(flags: Option<Value>) -> FsResult<bool> {
    match flags {
        None | Some(Value::Nil) => Ok(false),
        Some(Value::Integer(bits)) => Ok(bits & 1 != 0),
        Some(Value::Table(table)) => table
            .get::<Option<bool>>("excl")
            .map_err(|error| lua_error(&error))
            .map(|excl| excl.unwrap_or(false)),
        Some(other) => Err(FsError {
            name: "EINVAL",
            message: format!("invalid copyfile flags: {}", other.type_name()),
            raw_os_error: None,
        }),
    }
}

/// Resolves one `fs_utime`-family timestamp: a number of seconds, `"now"`,
/// or `nil`/`"omit"` keeping the current timestamp.
fn resolve_utime(value: Option<Value>, keep: FsTime) -> FsResult<FsTime> {
    match value {
        None | Some(Value::Nil) => Ok(keep),
        Some(Value::Integer(sec)) => Ok(FsTime { sec, nsec: 0 }),
        Some(Value::Number(seconds)) => Ok(split_seconds(seconds)),
        Some(Value::String(text)) => match text.to_str().map_err(|error| lua_error(&error))?.as_ref() {
            "now" => Ok(now_fs_time()),
            "omit" => Ok(keep),
            other => Err(FsError {
                name: "EINVAL",
                message: format!("invalid utime timestamp: {other}"),
                raw_os_error: None,
            }),
        },
        Some(other) => Err(FsError {
            name: "EINVAL",
            message: format!("invalid utime timestamp: {}", other.type_name()),
            raw_os_error: None,
        }),
    }
}

fn split_seconds(seconds: f64) -> FsTime {
    let duration = std::time::Duration::from_secs_f64(seconds.max(0.0));
    FsTime { sec: lua_int(duration.as_secs()), nsec: duration.subsec_nanos() }
}

fn now_fs_time() -> FsTime {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    FsTime { sec: lua_int(now.as_secs()), nsec: now.subsec_nanos() }
}

fn fs_time_table(lua: &Lua, time: FsTime) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("sec", time.sec)?;
    table.set("nsec", i64::from(time.nsec))?;
    Ok(table)
}

/// Builds the `uv.fs_stat()` result table documented in luvref.txt.
fn stat_table(lua: &Lua, stat: &Stat) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("dev", lua_int(stat.dev))?;
    table.set("mode", i64::from(stat.mode))?;
    table.set("nlink", lua_int(stat.nlink))?;
    table.set("uid", i64::from(stat.uid))?;
    table.set("gid", i64::from(stat.gid))?;
    table.set("rdev", lua_int(stat.rdev))?;
    table.set("ino", lua_int(stat.ino))?;
    table.set("size", lua_int(stat.size))?;
    table.set("blksize", lua_int(stat.blksize))?;
    table.set("blocks", lua_int(stat.blocks))?;
    table.set("flags", lua_int(stat.flags))?;
    table.set("gen", lua_int(stat.r#gen))?;
    table.set("atime", fs_time_table(lua, stat.atime)?)?;
    table.set("mtime", fs_time_table(lua, stat.mtime)?)?;
    table.set("ctime", fs_time_table(lua, stat.ctime)?)?;
    table.set("birthtime", fs_time_table(lua, stat.birthtime)?)?;
    table.set("type", stat_type_name(stat.mode))?;
    Ok(table)
}

/// Builds the `uv.fs_statfs()` result table documented in luvref.txt.
fn statfs_table(lua: &Lua, stats: &StatFs) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("type", lua_int(stats.kind))?;
    table.set("bsize", lua_int(stats.block_size))?;
    table.set("blocks", lua_int(stats.blocks))?;
    table.set("bfree", lua_int(stats.blocks_free))?;
    table.set("bavail", lua_int(stats.blocks_available))?;
    table.set("files", lua_int(stats.files))?;
    table.set("ffree", lua_int(stats.files_free))?;
    Ok(table)
}

/// Classifies a `stat` mode word into the luv type-name string.
fn stat_type_name(mode: u32) -> &'static str {
    match mode & 0o170_000 {
        0o140_000 => "socket",
        0o120_000 => "link",
        0o100_000 => "file",
        0o060_000 => "block",
        0o040_000 => "directory",
        0o020_000 => "char",
        0o010_000 => "fifo",
        _ => "unknown",
    }
}

fn entry_type_name(kind: DirEntryType) -> &'static str {
    match kind {
        DirEntryType::File => "file",
        DirEntryType::Directory => "directory",
        DirEntryType::Symlink => "link",
        DirEntryType::Fifo => "fifo",
        DirEntryType::Socket => "socket",
        DirEntryType::Character => "char",
        DirEntryType::Block => "block",
        DirEntryType::Unknown => "unknown",
    }
}

fn schedule_callback(
    scheduler: &Rc<dyn Scheduler>,
    lua: &Lua,
    callback: Function,
    args: MultiValue,
    fast: &FastCallbackState,
) -> Result<(), String> {
    let scheduler = Rc::clone(scheduler);
    let lua = lua.clone();
    let fast = fast.clone();
    scheduler.schedule_deferred(Box::new(move || {
        let _guard = fast.enter();
        call_with_traceback(&lua, &callback, args).map(|_| ())
    }))
}
