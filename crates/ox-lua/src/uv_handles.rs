//! Stream, process, DNS, work, and isolated-thread `vim.uv` bindings.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use mlua::{
    AnyUserData, Function, Lua, LuaString, MultiValue, Table, UserData, UserDataMethods, Value,
    Variadic,
};
use ox_uv::dns::{self, AddrInfoHints};
use ox_uv::fs::{FsError, FsResult};
use ox_uv::fs_watch::{FsEvent, FsEventOptions, FsEventRecord, WatchError};
use ox_uv::net::{NetEvent, Tcp, Udp};
#[cfg(unix)]
use ox_uv::net::{Pipe, Tty, TtyMode};
use ox_uv::process::{self, Process, ProcessPipe, SpawnOptions, StdioConfig};
use ox_uv::thread;
use ox_uv::{
    Async, CallbackError, Check, Handle, HandleId, Idle, Prepare, RunMode, Signal, Timer, UvLoop,
};

#[cfg(unix)]
const SIGNALS: &[(&str, i32)] = &[
    ("sighup", signal_hook::consts::SIGHUP),
    ("sigint", signal_hook::consts::SIGINT),
    ("sigquit", signal_hook::consts::SIGQUIT),
    ("sigill", signal_hook::consts::SIGILL),
    ("sigtrap", signal_hook::consts::SIGTRAP),
    ("sigabrt", signal_hook::consts::SIGABRT),
    ("sigbus", signal_hook::consts::SIGBUS),
    ("sigfpe", signal_hook::consts::SIGFPE),
    ("sigkill", signal_hook::consts::SIGKILL),
    ("sigusr1", signal_hook::consts::SIGUSR1),
    ("sigsegv", signal_hook::consts::SIGSEGV),
    ("sigusr2", signal_hook::consts::SIGUSR2),
    ("sigpipe", signal_hook::consts::SIGPIPE),
    ("sigalrm", signal_hook::consts::SIGALRM),
    ("sigterm", signal_hook::consts::SIGTERM),
    ("sigchld", signal_hook::consts::SIGCHLD),
    ("sigcont", signal_hook::consts::SIGCONT),
    ("sigstop", signal_hook::consts::SIGSTOP),
    ("sigtstp", signal_hook::consts::SIGTSTP),
    ("sigttin", signal_hook::consts::SIGTTIN),
    ("sigttou", signal_hook::consts::SIGTTOU),
    ("sigurg", signal_hook::consts::SIGURG),
    ("sigxcpu", signal_hook::consts::SIGXCPU),
    ("sigxfsz", signal_hook::consts::SIGXFSZ),
    ("sigvtalrm", signal_hook::consts::SIGVTALRM),
    ("sigprof", signal_hook::consts::SIGPROF),
    ("sigwinch", signal_hook::consts::SIGWINCH),
    ("sigio", signal_hook::consts::SIGIO),
    ("sigsys", signal_hook::consts::SIGSYS),
    ("sigiot", signal_hook::consts::SIGABRT),
    ("sigpoll", signal_hook::consts::SIGIO),
];

fn signal_number(value: Value) -> mlua::Result<i32> {
    match value {
        Value::Integer(number) => i32::try_from(number)
            .map_err(|_| mlua::Error::runtime(format!("invalid signal number: {number}"))),
        Value::String(name) => {
            let name = name.to_str()?;
            #[cfg(unix)]
            if let Some((_, number)) = SIGNALS.iter().find(|(candidate, _)| name == *candidate) {
                return Ok(*number);
            }
            Err(mlua::Error::runtime(format!("invalid signal name: {name}")))
        }
        _ => Err(mlua::Error::runtime("signal must be a string or integer")),
    }
}

fn signal_name(lua: &Lua, number: i32) -> mlua::Result<Value> {
    #[cfg(unix)]
    if let Some((name, _)) = SIGNALS.iter().find(|(_, candidate)| *candidate == number) {
        return Ok(Value::String(lua.create_string(*name)?));
    }
    Ok(Value::Integer(i64::from(number)))
}

use crate::vim::{FastCallbackState, Scheduler, call_with_traceback};

type DeferredOperation = Box<dyn FnOnce(&mut UvLoop)>;
type AfterRun = Rc<dyn Fn() -> mlua::Result<()>>;

#[derive(Clone)]
pub(crate) struct LoopAccess {
    pub(crate) uv_loop: Rc<RefCell<UvLoop>>,
    in_callback: Rc<Cell<bool>>,
    draining: Rc<Cell<bool>>,
    active_loop: Rc<Cell<Option<NonNull<UvLoop>>>>,
    deferred: Rc<RefCell<VecDeque<DeferredOperation>>>,
    after_run: Rc<RefCell<Option<AfterRun>>>,
}

/// RAII frame restoring `in_callback` and `active_loop` to their exact prior
/// state on drop — including unwind, so a panicking callback cannot leave a
/// stale `in_callback = true` or a dangling `active_loop` pointer behind.
struct CallbackFrame {
    in_callback: Rc<Cell<bool>>,
    active_loop: Rc<Cell<Option<NonNull<UvLoop>>>>,
    prior_callback: bool,
    prior_loop: Option<NonNull<UvLoop>>,
}

impl Drop for CallbackFrame {
    fn drop(&mut self) {
        self.active_loop.set(self.prior_loop);
        self.in_callback.set(self.prior_callback);
    }
}

/// RAII frame restoring `draining` on drop, so a panic during deferred-drain
/// cannot leave `draining = true` stuck and silently suppress future drains.
struct DrainingFrame {
    draining: Rc<Cell<bool>>,
    prior: bool,
}

impl Drop for DrainingFrame {
    fn drop(&mut self) {
        self.draining.set(self.prior);
    }
}

/// RAII guard restoring `in_callback` on drop, so a panicking deferred
/// operation cannot leave `in_callback = true` stuck mid-drain.
struct InCallbackGuard {
    in_callback: Rc<Cell<bool>>,
    prior: bool,
}

impl Drop for InCallbackGuard {
    fn drop(&mut self) {
        self.in_callback.set(self.prior);
    }
}

impl LoopAccess {
    pub(crate) fn new(uv_loop: Rc<RefCell<UvLoop>>) -> Self {
        Self {
            uv_loop,
            in_callback: Rc::new(Cell::new(false)),
            draining: Rc::new(Cell::new(false)),
            active_loop: Rc::new(Cell::new(None)),
            deferred: Rc::new(RefCell::new(VecDeque::new())),
            after_run: Rc::new(RefCell::new(None)),
        }
    }

    pub(crate) fn set_after_run(&self, after_run: AfterRun) {
        *self.after_run.borrow_mut() = Some(after_run);
    }

    pub(crate) fn apply(&self, operation: DeferredOperation) {
        if self.in_callback.get() {
            self.deferred.borrow_mut().push_back(operation);
            return;
        }
        operation(&mut self.uv_loop.borrow_mut());
    }
    /// Runs `operation` against the event loop. Inside a uv callback the
    /// callback-scoped `active_loop` pointer is used; outside, the shared
    /// `RefCell` is borrowed. This keeps mutating calls from panicking when
    /// the loop is already mutably borrowed by `run` or `run_once`.
    pub(crate) fn with_loop<R>(&self, operation: impl FnOnce(&mut UvLoop) -> R) -> R {
        if let Some(mut active_loop) = self.active_loop.get() {
            // Sound for the same callback-scoped reason documented in `run`.
            operation(unsafe { active_loop.as_mut() })
        } else {
            operation(&mut self.uv_loop.borrow_mut())
        }
    }

    /// Reads loop state through the callback-scoped pointer when one exists.
    /// Outside callbacks, a failed borrow is reported instead of being
    /// converted into a fabricated status value.
    pub(crate) fn with_loop_ref<R>(
        &self,
        operation: impl FnOnce(&UvLoop) -> R,
    ) -> mlua::Result<R> {
        if let Some(active_loop) = self.active_loop.get() {
            // SAFETY: `active_loop` is installed only for the synchronous
            // lifetime of the `&mut UvLoop` supplied to `callback`.
            return Ok(operation(unsafe { active_loop.as_ref() }));
        }
        let uv_loop = self.uv_loop.try_borrow().map_err(|_| {
            mlua::Error::runtime("vim.uv handle status is unavailable during callback")
        })?;
        Ok(operation(&uv_loop))
    }

    pub(crate) fn callback<R>(&self, uv_loop: &mut UvLoop, callback: impl FnOnce() -> R) -> R {
        let prior_callback = self.in_callback.replace(true);
        let prior_loop = self.active_loop.replace(Some(NonNull::from(&mut *uv_loop)));
        let frame = CallbackFrame {
            in_callback: Rc::clone(&self.in_callback),
            active_loop: Rc::clone(&self.active_loop),
            prior_callback,
            prior_loop,
        };
        let result = callback();
        // Restore callback/active-loop state before draining so the drain sees
        // the same state the original code did. Drop runs on unwind too.
        drop(frame);
        if !self.draining.get() {
            self.drain_deferred(uv_loop);
        }
        result
    }

    fn drain_deferred(&self, uv_loop: &mut UvLoop) {
        let prior_draining = self.draining.replace(true);
        let _draining = DrainingFrame {
            draining: Rc::clone(&self.draining),
            prior: prior_draining,
        };
        loop {
            let operation = self.deferred.borrow_mut().pop_front();
            let Some(operation) = operation else { break };
            let prior_callback = self.in_callback.replace(true);
            let _guard = InCallbackGuard {
                in_callback: Rc::clone(&self.in_callback),
                prior: prior_callback,
            };
            operation(uv_loop);
            // `_guard` restores `in_callback` at end of iteration (and on unwind).
        }
        // `_draining` restores `draining` on drop (and on unwind).
    }

    fn run(&self, mode: RunMode) -> mlua::Result<bool> {
        let alive = if let Some(mut active_loop) = self.active_loop.get() {
            // `callback` installs this pointer only for the synchronous lifetime
            // of the exact `&mut UvLoop` supplied by ox-uv.
            let uv_loop = unsafe { active_loop.as_mut() };
            self.drain_deferred(uv_loop);
            uv_loop.run_nested(mode).map_err(mlua::Error::external)?
        } else {
            self.uv_loop
                .borrow_mut()
                .run(mode)
                .map_err(mlua::Error::external)?
        };
        self.finish_run()?;
        Ok(alive)
    }

    pub(crate) fn poll(&self, timeout: i64) -> mlua::Result<()> {
        if let Some(mut active_loop) = self.active_loop.get() {
            // Sound for the same callback-scoped reason documented in `run`.
            let uv_loop = unsafe { active_loop.as_mut() };
            self.drain_deferred(uv_loop);
            poll_loop(uv_loop, timeout, true)?;
        } else {
            poll_loop(&mut self.uv_loop.borrow_mut(), timeout, false)?;
        }
        self.finish_run()
    }

    pub(crate) fn finish_run(&self) -> mlua::Result<()> {
        let after_run = self.after_run.borrow().clone();
        match after_run {
            Some(after_run) => after_run(),
            None => Ok(()),
        }
    }
}

fn poll_loop(uv_loop: &mut UvLoop, timeout: i64, nested: bool) -> mlua::Result<()> {
    let timeout_timer = if timeout >= 0 {
        let timeout = u64::try_from(timeout).map_err(mlua::Error::external)?;
        let timer = Timer::new(uv_loop).map_err(mlua::Error::external)?;
        timer
            .start(uv_loop, timeout, 0, |_, _| Ok(()))
            .map_err(mlua::Error::external)?;
        Some(timer)
    } else {
        None
    };
    if nested {
        uv_loop.run_nested(RunMode::Once)
    } else {
        uv_loop.run(RunMode::Once)
    }
    .map_err(mlua::Error::external)?;
    if let Some(timer) = timeout_timer {
        timer.close(uv_loop).map_err(mlua::Error::external)?;
        if nested {
            uv_loop.run_nested(RunMode::NoWait)
        } else {
            uv_loop.run(RunMode::NoWait)
        }
        .map_err(mlua::Error::external)?;
    }
    Ok(())
}

fn invoke(lua: &Lua, fast: &FastCallbackState, callback: &Function, args: MultiValue) {
    let _guard = fast.enter();
    if let Err(error) = call_with_traceback(lua, callback, args) {
        eprintln!("vim.uv callback error: {error}");
    }
}

fn error_args(lua: &Lua, result: Result<(), impl ToString>) -> mlua::Result<MultiValue> {
    let mut args = MultiValue::new();
    match result {
        Ok(()) => args.push_back(Value::Nil),
        Err(error) => args.push_back(Value::String(lua.create_string(error.to_string())?)),
    }
    Ok(args)
}

fn address_table(lua: &Lua, address: SocketAddr) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("ip", address.ip().to_string())?;
    table.set("port", address.port())?;
    table.set("family", if address.is_ipv4() { "inet" } else { "inet6" })?;
    Ok(table)
}

fn socket_addr(host: &str, port: u16) -> mlua::Result<SocketAddr> {
    let ip = host.parse::<IpAddr>().map_err(mlua::Error::external)?;
    Ok(SocketAddr::new(ip, port))
}

#[derive(Default)]
struct StreamCallbacks {
    listen: Option<Function>,
    connect: Option<Function>,
    read: Option<Function>,
    writes: HashMap<u64, Function>,
    /// Pipe-only: callback of the write being queued; a synchronously flushed
    /// completion claims it during `ProcessPipe::write` before its id is known.
    pending_write: Option<Function>,
    /// Pipe-only: set while a `ProcessPipe::write` call holds the pipe borrow.
    /// Every completion it delivers — this write or an earlier buffered one
    /// flushed by it — parks instead of invoking Lua under the borrow.
    write_in_flight: bool,
    /// Pipe-only: (callback, result) parked in completion order while a write
    /// call is in flight; delivered in order once the pipe borrow is released.
    parked_writes: VecDeque<(Function, Result<(), String>)>,
    shutdown: Option<Function>,
    accepted_tcp: VecDeque<Tcp>,
    #[cfg(unix)]
    accepted_pipe: VecDeque<Pipe>,
}

struct TcpContext {
    lua: Lua,
    fast: FastCallbackState,
    access: LoopAccess,
    routes: RefCell<HashMap<HandleId, Rc<RefCell<StreamCallbacks>>>>,
}

impl TcpContext {
    fn event(self: &Rc<Self>, uv_loop: &mut UvLoop, id: HandleId, event: NetEvent) {
        let callbacks = self.routes.borrow().get(&id).cloned();
        let Some(callbacks) = callbacks else { return };
        self.access.callback(uv_loop, || match event {
            NetEvent::AcceptedTcp(child) => {
                let callback = {
                    let mut state = callbacks.borrow_mut();
                    state.accepted_tcp.push_back(*child);
                    state.listen.clone()
                };
                if let Some(callback) = callback {
                    let mut args = MultiValue::new();
                    args.push_back(Value::Nil);
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            #[cfg(unix)]
            NetEvent::AcceptedPipe(child) => {
                let callback = {
                    let mut state = callbacks.borrow_mut();
                    state.accepted_pipe.push_back(*child);
                    state.listen.clone()
                };
                if let Some(callback) = callback {
                    let mut args = MultiValue::new();
                    args.push_back(Value::Nil);
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            NetEvent::Connected(result) => {
                let callback = callbacks.borrow_mut().connect.take();
                if let Some(callback) = callback
                    && let Ok(args) = error_args(&self.lua, result)
                {
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            NetEvent::Read(bytes) => {
                let callback = callbacks.borrow().read.clone();
                if let Some(callback) = callback
                    && let Ok(string) = self.lua.create_string(bytes)
                {
                    let mut args = MultiValue::new();
                    args.push_back(Value::Nil);
                    args.push_back(Value::String(string));
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            NetEvent::Eof => {
                let callback = callbacks.borrow().read.clone();
                if let Some(callback) = callback {
                    let mut args = MultiValue::new();
                    args.push_back(Value::Nil);
                    args.push_back(Value::Nil);
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            NetEvent::WriteComplete { id, result } => {
                let callback = callbacks.borrow_mut().writes.remove(&id.get());
                if let Some(callback) = callback
                    && let Ok(args) = error_args(&self.lua, result)
                {
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            NetEvent::ShutdownComplete(result) => {
                let callback = callbacks.borrow_mut().shutdown.take();
                if let Some(callback) = callback
                    && let Ok(args) = error_args(&self.lua, result)
                {
                    invoke(&self.lua, &self.fast, &callback, args);
                }
            }
            NetEvent::Error(error) => {
                let callback = {
                    let state = callbacks.borrow();
                    state
                        .read
                        .clone()
                        .or_else(|| state.connect.clone())
                        .or_else(|| state.listen.clone())
                };
                if let Some(callback) = callback {
                    let mut args = MultiValue::new();
                    if let Ok(message) = self.lua.create_string(error.to_string()) {
                        args.push_back(Value::String(message));
                        args.push_back(Value::Nil);
                        invoke(&self.lua, &self.fast, &callback, args);
                    }
                }
            }
            NetEvent::Datagram { .. } => {}
        });
    }
}

#[derive(Clone)]
struct LuaTcp {
    inner: Rc<RefCell<Option<Tcp>>>,
    callbacks: Rc<RefCell<StreamCallbacks>>,
    context: Rc<TcpContext>,
    closing: Rc<Cell<bool>>,
}

impl UserData for LuaTcp {
    #[expect(
        clippy::too_many_lines,
        reason = "Lua TCP methods must be registered together on one userdata builder"
    )]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bind", |_, this, (host, port): (String, u16)| {
            let address = socket_addr(&host, port)?;
            let context = this.context.clone();
            let context_for_callback = context.clone();
            let tcp = Tcp::bind(
                &mut context.access.uv_loop.borrow_mut(),
                address,
                move |loop_, id, event| context_for_callback.event(loop_, id, event),
            )
            .map_err(mlua::Error::external)?;
            context
                .routes
                .borrow_mut()
                .insert(tcp.id(), this.callbacks.clone());
            *this.inner.borrow_mut() = Some(tcp);
            Ok(true)
        });
        methods.add_method(
            "connect",
            |_, this, (host, port, callback): (String, u16, Function)| {
                this.callbacks.borrow_mut().connect = Some(callback);
                let address = socket_addr(&host, port)?;
                let inner = this.inner.clone();
                let callbacks = this.callbacks.clone();
                let context = this.context.clone();
                let access = context.access.clone();
                let context_for_callback = context.clone();
                access.apply(Box::new(move |uv_loop| {
                    match Tcp::connect(uv_loop, address, move |loop_, id, event| {
                        context_for_callback.event(loop_, id, event);
                    }) {
                        Ok(tcp) => {
                            context.routes.borrow_mut().insert(tcp.id(), callbacks);
                            *inner.borrow_mut() = Some(tcp);
                        }
                        Err(error) => {
                            if let Some(callback) = callbacks.borrow_mut().connect.take()
                                && let Ok(args) = error_args(&context.lua, Err(error))
                            {
                                invoke(&context.lua, &context.fast, &callback, args);
                            }
                        }
                    }
                }));
                Ok(true)
            },
        );
        methods.add_method("listen", |_, this, (backlog, callback): (u32, Function)| {
            this.callbacks.borrow_mut().listen = Some(callback);
            let inner = this.inner.clone();
            this.context.access.apply(Box::new(move |uv_loop| {
                if let Some(tcp) = inner.borrow_mut().as_mut() {
                    let _ = tcp.listen(uv_loop, backlog);
                }
            }));
            Ok(true)
        });
        methods.add_method("accept", |_, this, peer: AnyUserData| {
            let peer = peer.borrow_mut::<LuaTcp>()?;
            let child = this
                .callbacks
                .borrow_mut()
                .accepted_tcp
                .pop_front()
                .ok_or_else(|| mlua::Error::runtime("no pending TCP connection"))?;
            this.context
                .routes
                .borrow_mut()
                .insert(child.id(), peer.callbacks.clone());
            *peer.inner.borrow_mut() = Some(child);
            Ok(true)
        });
        add_tcp_stream_methods(methods);
        methods.add_method("getsockname", |lua, this, ()| {
            let inner = this.inner.borrow();
            let tcp = inner
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("TCP handle is not initialized"))?;
            address_table(lua, tcp.local_addr().map_err(mlua::Error::external)?)
        });
        methods.add_method("getpeername", |lua, this, ()| {
            let inner = this.inner.borrow();
            let tcp = inner
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("TCP handle is not initialized"))?;
            address_table(lua, tcp.peer_addr().map_err(mlua::Error::external)?)
        });
        methods.add_method("nodelay", |_, this, enable: bool| {
            let inner = this.inner.borrow();
            inner
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("TCP handle is not initialized"))?
                .nodelay(enable)
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("is_closing", |_, this, ()| {
            this.context.access.with_loop_ref(|uv_loop| {
                this.inner
                    .borrow()
                    .as_ref()
                    .is_none_or(|tcp| tcp.is_closing(uv_loop))
            })
        });
        methods.add_method("close", |_, this, callback: Option<Function>| {
            if this.closing.replace(true) {
                return Ok(());
            }
            let inner = this.inner.clone();
            let context = this.context.clone();
            let callbacks = this.callbacks.clone();
            let access = context.access.clone();
            access.apply(Box::new(move |uv_loop| {
                if let Some(tcp) = inner.borrow_mut().take() {
                    let id = tcp.id();
                    let _ = tcp.close(uv_loop);
                    context.routes.borrow_mut().remove(&id);
                    *callbacks.borrow_mut() = StreamCallbacks::default();
                    if let Some(callback) = callback {
                        invoke(&context.lua, &context.fast, &callback, MultiValue::new());
                    }
                }
            }));
            Ok(())
        });
    }
}

impl Drop for LuaTcp {
    /// luv `__gc`: a collected handle closes its stream so the Rust event
    /// closure (and every callback reference it holds) is released instead of
    /// pinning auxiliary-stack slots for the lifetime of the loop.
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) != 1 || self.closing.replace(true) {
            return;
        }
        let inner = self.inner.clone();
        let context = self.context.clone();
        let callbacks = self.callbacks.clone();
        let access = context.access.clone();
        access.apply(Box::new(move |uv_loop| {
            if let Some(tcp) = inner.borrow_mut().take() {
                let id = tcp.id();
                let _ = tcp.close(uv_loop);
                context.routes.borrow_mut().remove(&id);
                *callbacks.borrow_mut() = StreamCallbacks::default();
            }
        }));
    }
}

fn add_tcp_stream_methods<M: UserDataMethods<LuaTcp>>(methods: &mut M) {
    methods.add_method("read_start", |_, this, callback: Function| {
        this.callbacks.borrow_mut().read = Some(callback);
        let inner = this.inner.clone();
        this.context.access.apply(Box::new(move |uv_loop| {
            if let Some(tcp) = inner.borrow_mut().as_mut() {
                let _ = tcp.read_start(uv_loop);
            }
        }));
        Ok(true)
    });
    methods.add_method("read_stop", |_, this, ()| {
        this.callbacks.borrow_mut().read = None;
        let inner = this.inner.clone();
        this.context.access.apply(Box::new(move |uv_loop| {
            if let Some(tcp) = inner.borrow_mut().as_mut() {
                let _ = tcp.read_stop(uv_loop);
            }
        }));
        Ok(true)
    });
    methods.add_method(
        "write",
        |_, this, (bytes, callback): (LuaString, Option<Function>)| {
            let data = bytes.as_bytes().to_vec();
            let inner = this.inner.clone();
            let callbacks = this.callbacks.clone();
            this.context.access.apply(Box::new(move |uv_loop| {
                if let Some(tcp) = inner.borrow_mut().as_mut()
                    && let Ok(id) = tcp.write(uv_loop, data)
                    && let Some(callback) = callback
                {
                    callbacks.borrow_mut().writes.insert(id.get(), callback);
                }
            }));
            Ok(true)
        },
    );
    methods.add_method("shutdown", |_, this, callback: Option<Function>| {
        this.callbacks.borrow_mut().shutdown = callback;
        let inner = this.inner.clone();
        this.context.access.apply(Box::new(move |uv_loop| {
            if let Some(tcp) = inner.borrow_mut().as_mut() {
                let _ = tcp.shutdown(uv_loop);
            }
        }));
        Ok(true)
    });
}

#[cfg(unix)]
#[derive(Clone)]
struct LuaProcessPipe {
    inner: Rc<RefCell<Option<ProcessPipe>>>,
    callbacks: Rc<RefCell<StreamCallbacks>>,
    lua: Lua,
    fast: FastCallbackState,
    access: LoopAccess,
    closing: Rc<Cell<bool>>,
}

#[cfg(unix)]
impl LuaProcessPipe {
    fn install_endpoint(&self, mut pipe: ProcessPipe) {
        let callbacks = self.callbacks.clone();
        let lua = self.lua.clone();
        let fast = self.fast.clone();
        let access = self.access.clone();
        pipe.set_callback(move |uv_loop, _, event| {
            access.callback(uv_loop, || match event {
                NetEvent::Read(bytes) => {
                    if let Some(callback) = callbacks.borrow().read.clone()
                        && let Ok(string) = lua.create_string(bytes)
                    {
                        let mut args = MultiValue::new();
                        args.push_back(Value::Nil);
                        args.push_back(Value::String(string));
                        invoke(&lua, &fast, &callback, args);
                    }
                }
                NetEvent::Eof => {
                    if let Some(callback) = callbacks.borrow().read.clone() {
                        let mut args = MultiValue::new();
                        args.push_back(Value::Nil);
                        args.push_back(Value::Nil);
                        invoke(&lua, &fast, &callback, args);
                    }
                }
                // Release the borrow before invoking: the Lua write callback may
                // call back into this pipe (shutdown/close/write).
                NetEvent::WriteComplete { id, result } => {
                    let result = result.map_err(|error| error.to_string());
                    let mut state = callbacks.borrow_mut();
                    let callback = state
                        .writes
                        .remove(&id.get())
                        .or_else(|| state.pending_write.take());
                    match callback {
                        // A completion delivered inside `ProcessPipe::write` — for
                        // the write being queued or an earlier buffered one it just
                        // flushed: park it; the pipe borrow is still held.
                        Some(callback) if state.write_in_flight => {
                            state.parked_writes.push_back((callback, result));
                        }
                        Some(callback) => {
                            drop(state);
                            if let Ok(args) = error_args(&lua, result) {
                                invoke(&lua, &fast, &callback, args);
                            }
                        }
                        None => {}
                    }
                }
                _ => {}
            });
        });
        *self.inner.borrow_mut() = Some(pipe);
    }
}

#[cfg(unix)]
impl Drop for LuaProcessPipe {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) != 1 || self.closing.replace(true) {
            return;
        }
        let Some(pipe) = self.inner.borrow_mut().take() else {
            return;
        };
        self.access.apply(Box::new(move |uv_loop| {
            let _ = pipe.close(uv_loop);
        }));
    }
}

#[cfg(unix)]
impl UserData for LuaProcessPipe {
    #[expect(
        clippy::too_many_lines,
        reason = "pipe method closures share the parked-write state machine; registration is one dispatch unit"
    )]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("read_start", |_, this, callback: Function| {
            this.callbacks.borrow_mut().read = Some(callback);
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(pipe) = inner.borrow_mut().as_mut() {
                    let _ = pipe.read_start_current(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method("read_stop", |_, this, ()| {
            this.callbacks.borrow_mut().read = None;
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(pipe) = inner.borrow_mut().as_mut() {
                    let _ = pipe.read_stop(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method(
            "write",
            |_, this, (bytes, callback): (LuaString, Option<Function>)| {
                let data = bytes.as_bytes().to_vec();
                let inner = this.inner.clone();
                let callbacks = this.callbacks.clone();
                let lua = this.lua.clone();
                let fast = this.fast.clone();
                let access = this.access.clone();
                this.access.apply(Box::new(move |uv_loop| {
                    if let Some(callback) = callback {
                        callbacks.borrow_mut().pending_write = Some(callback);
                    }
                    // `ProcessPipe::write` flushes synchronously and delivers
                    // WriteComplete events inside this call — for the write being
                    // queued and for any earlier buffered writes it drains. Park
                    // them all; keep Lua out until the pipe borrow is released.
                    let queued = {
                        callbacks.borrow_mut().write_in_flight = true;
                        let queued = {
                            let mut inner = inner.borrow_mut();
                            match inner.as_mut() {
                                Some(pipe) => pipe.write(uv_loop, data),
                                None => Err(ox_uv::net::NetError::Closed),
                            }
                        };
                        callbacks.borrow_mut().write_in_flight = false;
                        queued
                    };
                    let claimed = callbacks.borrow_mut().pending_write.take();
                    match (queued, claimed) {
                        // An outstanding remainder completes on a later loop turn
                        // under this write id, like TCP/TTY.
                        (Ok(id), Some(callback)) => {
                            callbacks.borrow_mut().writes.insert(id.get(), callback);
                        }
                        // The queued write never reached the pipe: report it here.
                        (Err(error), Some(callback)) => {
                            callbacks
                                .borrow_mut()
                                .parked_writes
                                .push_back((callback, Err(error.to_string())));
                        }
                        _ => {}
                    }
                    let parked: Vec<_> = callbacks.borrow_mut().parked_writes.drain(..).collect();
                    if !parked.is_empty() {
                        access.callback(uv_loop, move || {
                            for (callback, result) in parked {
                                if let Ok(args) = error_args(&lua, result) {
                                    invoke(&lua, &fast, &callback, args);
                                }
                            }
                        });
                    }
                }));
                Ok(true)
            },
        );
        methods.add_method("shutdown", |_, this, callback: Option<Function>| {
            this.callbacks.borrow_mut().shutdown = callback;
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(pipe) = inner.borrow_mut().as_mut() {
                    let _ = pipe.shutdown(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method("close", |_, this, callback: Option<Function>| {
            if this.closing.replace(true) {
                return Ok(());
            }
            let Some(pipe) = this.inner.borrow_mut().take() else {
                return Ok(());
            };
            let lua = this.lua.clone();
            let fast = this.fast.clone();
            let callbacks = this.callbacks.clone();
            this.access.apply(Box::new(move |uv_loop| {
                let _ = pipe.close(uv_loop);
                *callbacks.borrow_mut() = StreamCallbacks::default();
                if let Some(callback) = callback {
                    invoke(&lua, &fast, &callback, MultiValue::new());
                }
            }));
            Ok(())
        });
        methods.add_method("is_closing", |_, this, ()| {
            Ok(this.closing.get() || this.inner.borrow().is_none())
        });
    }
}

#[derive(Clone)]
struct LuaProcess {
    inner: Rc<RefCell<Option<Process>>>,
    access: LoopAccess,
    closing: Rc<Cell<bool>>,
}

impl Drop for LuaProcess {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) != 1 || self.closing.replace(true) {
            return;
        }
        let inner = self.inner.clone();
        self.access.apply(Box::new(move |uv_loop| {
            if let Some(process) = inner.borrow_mut().take() {
                let _ = process.close(uv_loop);
            }
        }));
    }
}

impl UserData for LuaProcess {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get_pid", |_, this, ()| {
            Ok(this.inner.borrow().as_ref().map(Process::pid))
        });
        methods.add_method("kill", |_, this, signal: Option<i32>| {
            this.inner
                .borrow()
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("process is closed"))?
                .kill(signal)
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("close", |_, this, ()| {
            if this.closing.replace(true) {
                return Ok(());
            }
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(process) = inner.borrow_mut().take() {
                    let _ = process.close(uv_loop);
                }
            }));
            Ok(())
        });
        methods.add_method("is_closing", |_, this, ()| {
            Ok(this.closing.get() || this.inner.borrow().is_none())
        });
    }
}

#[derive(Clone)]
enum ThreadArg {
    Nil,
    Bool(bool),
    Integer(i64),
    Number(f64),
    String(Vec<u8>),
}
fn thread_arg(value: Value) -> mlua::Result<ThreadArg> {
    match value {
        Value::Nil => Ok(ThreadArg::Nil),
        Value::Boolean(v) => Ok(ThreadArg::Bool(v)),
        Value::Integer(v) => Ok(ThreadArg::Integer(v)),
        Value::Number(v) => Ok(ThreadArg::Number(v)),
        Value::String(v) => Ok(ThreadArg::String(v.as_bytes().to_vec())),
        other => Err(mlua::Error::runtime(format!(
            "unsupported thread argument {}",
            other.type_name()
        ))),
    }
}

fn push_thread_arg(lua: &Lua, values: &mut MultiValue, argument: ThreadArg) -> Result<(), String> {
    values.push_back(match argument {
        ThreadArg::Nil => Value::Nil,
        ThreadArg::Bool(value) => Value::Boolean(value),
        ThreadArg::Integer(value) => Value::Integer(value),
        ThreadArg::Number(value) => Value::Number(value),
        ThreadArg::String(value) => Value::String(
            lua.create_string(value)
                .map_err(|error| error.to_string())?,
        ),
    });
    Ok(())
}

fn run_isolated(chunk: &[u8], arguments: Vec<ThreadArg>) -> Result<Vec<ThreadArg>, String> {
    let child = Lua::new();
    let function = child
        .load(chunk)
        .into_function()
        .map_err(|error| error.to_string())?;
    let mut values = MultiValue::new();
    for argument in arguments {
        push_thread_arg(&child, &mut values, argument)?;
    }
    let returned = function
        .call::<MultiValue>(values)
        .map_err(|error| error.to_string())?;
    returned
        .into_iter()
        .map(|value| thread_arg(value).map_err(|error| error.to_string()))
        .collect()
}

#[derive(Default)]
struct WorkCompletion {
    results: Mutex<VecDeque<Result<Vec<ThreadArg>, String>>>,
}
struct PendingWork {
    completion: Arc<WorkCompletion>,
    callback: Function,
}
struct LuaWork {
    work: ox_uv::work::Work<ox_uv::UvLoopPoster>,
    /// Owns the after-work callback; dropped deterministically when this
    /// userdata is collected, releasing its auxiliary-stack reference.
    #[allow(dead_code)]
    pending: Rc<PendingWork>,
}
impl UserData for LuaWork {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("queue", |_, this, args: Variadic<Value>| {
            let arguments = args
                .into_iter()
                .map(thread_arg)
                .collect::<mlua::Result<Vec<_>>>()?;
            this.work
                .queue(Box::new(arguments))
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
    }
}
struct LuaThread {
    inner: RefCell<Option<thread::Thread<Result<(), String>>>>,
}
impl UserData for LuaThread {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("join", |_, this, ()| {
            let mut thread = this.inner.borrow_mut();
            let thread = thread
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("thread is closed"))?;
            thread
                .join()
                .map_err(mlua::Error::external)?
                .map_err(mlua::Error::runtime)?;
            Ok(true)
        });
        methods.add_method_mut("detach", |_, this, ()| {
            this.inner
                .borrow_mut()
                .as_mut()
                .ok_or_else(|| mlua::Error::runtime("thread is closed"))?
                .detach()
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
    }
}

#[derive(Default)]
struct ProcessCompletion {
    result: Mutex<Option<Result<(i64, i32), String>>>,
}
struct PendingProcess {
    completion: Arc<ProcessCompletion>,
    callback: Function,
}

#[derive(Clone, Copy)]
enum PhaseHandle {
    Idle(Idle),
    Prepare(Prepare),
    Check(Check),
}
impl PhaseHandle {
    fn start(
        self,
        uv_loop: &mut UvLoop,
        mut callback: impl FnMut(&mut UvLoop) -> Result<(), CallbackError> + 'static,
    ) -> ox_uv::Result<()> {
        match self {
            Self::Idle(handle) => handle.start(uv_loop, move |loop_, _| callback(loop_)),
            Self::Prepare(handle) => handle.start(uv_loop, move |loop_, _| callback(loop_)),
            Self::Check(handle) => handle.start(uv_loop, move |loop_, _| callback(loop_)),
        }
    }
    fn stop(self, uv_loop: &mut UvLoop) -> ox_uv::Result<()> {
        match self {
            Self::Idle(handle) => handle.stop(uv_loop),
            Self::Prepare(handle) => handle.stop(uv_loop),
            Self::Check(handle) => handle.stop(uv_loop),
        }
    }
    fn close(self, uv_loop: &mut UvLoop) -> ox_uv::Result<()> {
        match self {
            Self::Idle(handle) => handle.close(uv_loop),
            Self::Prepare(handle) => handle.close(uv_loop),
            Self::Check(handle) => handle.close(uv_loop),
        }
    }
    fn active(self, uv_loop: &UvLoop) -> bool {
        match self {
            Self::Idle(handle) => handle.is_active(uv_loop),
            Self::Prepare(handle) => handle.is_active(uv_loop),
            Self::Check(handle) => handle.is_active(uv_loop),
        }
    }
    fn closing(self, uv_loop: &UvLoop) -> bool {
        match self {
            Self::Idle(handle) => handle.is_closing(uv_loop),
            Self::Prepare(handle) => handle.is_closing(uv_loop),
            Self::Check(handle) => handle.is_closing(uv_loop),
        }
    }
}
struct LuaPhase {
    handle: PhaseHandle,
    access: LoopAccess,
    lua: Lua,
    fast: FastCallbackState,
}
impl UserData for LuaPhase {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("start", |_, this, callback: Function| {
            let handle = this.handle;
            let access = this.access.clone();
            let event_access = access.clone();
            let lua = this.lua.clone();
            let fast = this.fast.clone();
            access.apply(Box::new(move |uv_loop| {
                let _ = handle.start(uv_loop, move |loop_| {
                    event_access.callback(loop_, || {
                        invoke(&lua, &fast, &callback, MultiValue::new());
                    });
                    Ok(())
                });
            }));
            Ok(true)
        });
        methods.add_method("stop", |_, this, ()| {
            let handle = this.handle;
            this.access.apply(Box::new(move |uv_loop| {
                let _ = handle.stop(uv_loop);
            }));
            Ok(true)
        });
        methods.add_method("is_active", |_, this, ()| {
            this.access
                .with_loop_ref(|uv_loop| this.handle.active(uv_loop))
        });
        methods.add_method("is_closing", |_, this, ()| {
            this.access
                .with_loop_ref(|uv_loop| this.handle.closing(uv_loop))
        });
        methods.add_method("close", |_, this, ()| {
            let handle = this.handle;
            this.access.apply(Box::new(move |uv_loop| {
                let _ = handle.close(uv_loop);
            }));
            Ok(())
        });
    }
}

struct LuaAsync {
    handle: Async,
    access: LoopAccess,
}
impl UserData for LuaAsync {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("send", |_, this, ()| {
            // `callback()` restores `active_loop` before `drain_deferred`,
            // so a Lua invocation from a deferred operation sees
            // `in_callback=true` with no active pointer. `with_loop` would
            // re-borrow the RefCell already held by `run()`; `apply` uses
            // the deferred-operation pointer instead.
            if this.access.in_callback.get() && this.access.active_loop.get().is_none() {
                let handle = this.handle;
                this.access.apply(Box::new(move |uv_loop| {
                    let _ = handle.send(uv_loop);
                }));
            } else {
                this.access.with_loop(|uv_loop| {
                    this.handle.send(uv_loop).map_err(mlua::Error::external)
                })?;
            }
            Ok(true)
        });
        methods.add_method("close", |_, this, ()| {
            let handle = this.handle;
            this.access.apply(Box::new(move |uv_loop| {
                let _ = handle.close(uv_loop);
            }));
            Ok(())
        });
    }
}

struct LuaSignal {
    handle: Signal,
    access: LoopAccess,
    lua: Lua,
    fast: FastCallbackState,
}

impl LuaSignal {
    fn start(&self, signum: Value, callback: Function, oneshot: bool) -> mlua::Result<i32> {
        let signum = signal_number(signum)?;
        let handle = self.handle;
        let access = self.access.clone();
        let event_access = access.clone();
        let lua = self.lua.clone();
        let fast = self.fast.clone();
        access.apply(Box::new(move |uv_loop| {
            let event_lua = lua.clone();
            let event_fast = fast.clone();
            let event_callback = move |loop_: &mut UvLoop, _: HandleId, delivered: i32| {
                event_access.callback(loop_, || {
                    let mut args = MultiValue::new();
                    args.push_back(
                        signal_name(&event_lua, delivered)
                            .unwrap_or(Value::Integer(i64::from(delivered))),
                    );
                    invoke(&event_lua, &event_fast, &callback, args);
                });
                Ok(())
            };
            let _ = if oneshot {
                handle.start_oneshot(uv_loop, signum, event_callback)
            } else {
                handle.start(uv_loop, signum, event_callback)
            };
        }));
        Ok(0)
    }

    fn stop(&self) -> i32 {
        let handle = self.handle;
        self.access.apply(Box::new(move |uv_loop| {
            let _ = handle.stop(uv_loop);
        }));
        0
    }
}

impl UserData for LuaSignal {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("start", |_, this, (signum, callback): (Value, Function)| {
            this.start(signum, callback, false)
        });
        methods.add_method(
            "start_oneshot",
            |_, this, (signum, callback): (Value, Function)| this.start(signum, callback, true),
        );
        methods.add_method("stop", |_, this, ()| Ok(this.stop()));
        methods.add_method("is_closing", |_, this, ()| {
            this.access
                .with_loop_ref(|uv_loop| this.handle.is_closing(uv_loop))
        });
        methods.add_method("close", |_, this, ()| {
            let handle = this.handle;
            this.access.apply(Box::new(move |uv_loop| {
                let _ = handle.close(uv_loop);
            }));
            Ok(())
        });
    }
}

/// Queued filesystem event in `Send`-owned form. The watcher thread posts
/// records through the loop poster, but the Lua callback is `!Send`, so
/// delivery splits into a cross-thread queue plus a loop-thread drain in
/// the shared `after_run` hook (same shape as the process-exit drain).
type FsQueueItem = Result<(Vec<u8>, bool, bool), String>;

struct FsEventRoute {
    queue: Arc<Mutex<VecDeque<FsQueueItem>>>,
    callback: Function,
    /// The owning handle's phase cell. The drain rechecks it before every
    /// delivery so a callback that stopped or closed its own handle
    /// silences the rest of the batch even on the skipped-removal path
    /// where the route entry outlives the teardown.
    phase: Rc<Cell<FsEventPhase>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FsEventPhase {
    Idle,
    Active(u64),
    Closed,
}

struct LuaFsEvent {
    state: Rc<RefCell<Option<FsEvent>>>,
    access: LoopAccess,
    routes: Rc<RefCell<HashMap<u64, FsEventRoute>>>,
    next_id: Rc<Cell<u64>>,
    phase: Rc<Cell<FsEventPhase>>,
    wake: ox_uv::AsyncSender,
    /// Last successfully started path, kept across `stop` like luv keeps
    /// it, so `getpath` answers on a restartable handle too.
    path: RefCell<Option<Vec<u8>>>,
}

/// Lua strings are byte strings, so the native path encoding must bypass
/// UTF-8 conversion before crossing the filesystem-event callback boundary.
fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

fn path_from_bytes(bytes: &[u8]) -> mlua::Result<PathBuf> {
    #[cfg(unix)]
    {
        Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        let path = std::str::from_utf8(bytes)
            .map_err(|_| mlua::Error::runtime("path must be valid UTF-8"))?;
        Ok(PathBuf::from(path))
    }
}

fn watch_error_parts(error: WatchError) -> (&'static str, String) {
    match error {
        WatchError::Io(error) => (error.name, error.to_string()),
        WatchError::Post(error) => ("ECANCELED", format!("ECANCELED: {error}")),
        WatchError::Spawn(error) => (
            "EAGAIN",
            format!("EAGAIN: watcher thread could not be started: {error}"),
        ),
        WatchError::Stopped => ("EINVAL", "EINVAL: watcher has already stopped".to_owned()),
        WatchError::Unsupported(reason)
            if reason == "watch_entry cannot be combined with recursive traversal" =>
        {
            (
                "ENOTSUP",
                "ENOTSUP: watch_entry cannot be combined with recursive".to_owned(),
            )
        }
        WatchError::Unsupported(reason) => ("ENOTSUP", format!("ENOTSUP: {reason}")),
        WatchError::Loop(error) => match error {
            ox_uv::Error::Io(inner) => {
                let fs_error = FsError::from(inner);
                (fs_error.name, fs_error.to_string())
            }
            ox_uv::Error::MissingEnvironment(name) => (
                "ENOENT",
                format!("ENOENT: environment variable {name} is not set"),
            ),
            ox_uv::Error::Unsupported { .. } => ("ENOTSUP", format!("ENOTSUP: {error}")),
            _ => ("EINVAL", format!("EINVAL: {error}")),
        },
    }
}

fn watch_failure(lua: &Lua, error: WatchError) -> mlua::Result<MultiValue> {
    let (name, message) = watch_error_parts(error);
    Ok(MultiValue::from_vec(vec![
        Value::Nil,
        Value::String(lua.create_string(message)?),
        Value::String(lua.create_string(name)?),
    ]))
}

impl LuaFsEvent {
    fn option_flag(flags: &Table, key: &str) -> mlua::Result<bool> {
        match flags.get::<Value>(key)? {
            Value::Nil => Ok(false),
            Value::Boolean(value) => Ok(value),
            Value::Integer(value) => Ok(value != 0),
            Value::Number(value) => Ok(value != 0.0),
            _ => Err(mlua::Error::runtime(format!(
                "Invalid '{key}': not a boolean"
            ))),
        }
    }

    fn options(flags: &Table) -> mlua::Result<FsEventOptions> {
        Ok(FsEventOptions {
            watch_entry: Self::option_flag(flags, "watch_entry")?,
            stat: Self::option_flag(flags, "stat")?,
            recursive: Self::option_flag(flags, "recursive")?,
        })
    }

    fn start(
        &self,
        lua: &Lua,
        path: LuaString,
        flags: &Table,
        callback: Function,
    ) -> mlua::Result<MultiValue> {
        self.check_idle()?;
        let raw_path = path.as_bytes().to_vec();
        let path = path_from_bytes(&raw_path)?;
        // luv surfaces an unstartable path as `nil, err, name` so
        // `vim._watch` can notify on ENOENT. The synchronous backend start
        // below still uses this preflight to preserve luv's exact errno
        // tuple for an absent or inaccessible path.
        if let Err(error) = std::fs::metadata(&path) {
            let missing = error.kind() == io::ErrorKind::NotFound;
            let fs_error = FsError::from(error);
            let name = fs_error.name;
            let mut message = if missing {
                format!("{name}: no such file or directory: ").into_bytes()
            } else {
                format!("{name}: {msg}: ", msg = fs_error.message).into_bytes()
            };
            message.extend_from_slice(&raw_path);
            return Ok(MultiValue::from_vec(vec![
                Value::Nil,
                Value::String(lua.create_string(message)?),
                Value::String(lua.create_string(name)?),
            ]));
        }
        let options = Self::options(flags)?;
        // Flag lookups can invoke __index and reenter start or close.
        self.check_idle()?;
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        self.routes.borrow_mut().insert(
            id,
            FsEventRoute {
                queue: queue.clone(),
                callback,
                phase: self.phase.clone(),
            },
        );
        let state = self.state.clone();
        let fail_routes = self.routes.clone();
        let phase = self.phase.clone();
        let wake = self.wake.clone();
        let result = self.access.with_loop(move |uv_loop| {
            let event_callback = move |_: &mut UvLoop, result: FsResult<FsEventRecord>| {
                let item = match result {
                    Ok(record) => Ok((
                        path_bytes(&record.filename),
                        record.change,
                        record.rename,
                    )),
                    Err(error) => Err(error.to_string()),
                };
                if let Ok(mut pending) = queue.lock() {
                    pending.push_back(item);
                }
                let _ = wake.send();
            };
            match FsEvent::start(uv_loop, path, options, event_callback) {
                Ok(event) => {
                    *state.borrow_mut() = Some(event);
                    phase.set(FsEventPhase::Active(id));
                    Ok(())
                }
                Err(error) => {
                    fail_routes.borrow_mut().remove(&id);
                    phase.set(FsEventPhase::Idle);
                    Err(error)
                }
            }
        });
        match result {
            Ok(()) => {
                *self.path.borrow_mut() = Some(raw_path);
                Ok(MultiValue::from_vec(vec![Value::Integer(0)]))
            }
            Err(error) => watch_failure(lua, error),
        }
    }

    fn check_idle(&self) -> mlua::Result<()> {
        match self.phase.get() {
            FsEventPhase::Idle => Ok(()),
            FsEventPhase::Closed => Err(mlua::Error::runtime("fs event is closed")),
            FsEventPhase::Active(_) => Err(mlua::Error::runtime("fs event already started")),
        }
    }
    /// Also the `Drop` path, so every runtime-state borrow is `try_*`: no
    /// borrow of `routes`/`state` is ever held across a Lua call here, so a
    /// failure means reentrancy we cannot service — skipping beats poisoning
    /// the allocator during GC. The `Cell` transition always lands, which is
    /// what blocks resurrection.
    fn teardown(&self, target: FsEventPhase) {
        let previous = self.phase.replace(target);
        let FsEventPhase::Active(id) = previous else {
            // A closed handle is never restartable, not even via `stop`.
            if previous == FsEventPhase::Closed {
                self.phase.set(FsEventPhase::Closed);
            }
            return;
        };
        if let Ok(mut routes) = self.routes.try_borrow_mut() {
            routes.remove(&id);
        }
        // Move ownership now: a subsequent start must never be closed by this
        // deferred teardown, even when both operations originate in a callback.
        let event = self.state.try_borrow_mut().ok().and_then(|mut state| state.take());
        if let Some(event) = event {
            self.access.apply(Box::new(move |uv_loop| {
                let _ = event.close(uv_loop);
            }));
        }
    }

    fn close_handle(&self) {
        self.teardown(FsEventPhase::Closed);
    }

    fn stop_watching(&self) -> i32 {
        self.teardown(FsEventPhase::Idle);
        0
    }

    /// luv's `fs_event:getpath()`: the monitored path on success, luv's
    /// `nil, err, name` fail shape for a handle that never started, and the
    /// method form's exact "fs event is closed" refusal for a closed one —
    /// the same string `start` raises there.
    fn getpath(&self, lua: &Lua) -> mlua::Result<MultiValue> {
        if self.phase.get() == FsEventPhase::Closed {
            return Err(mlua::Error::runtime("fs event is closed"));
        }
        let Some(path) = self.path.borrow().clone() else {
            return Ok(MultiValue::from_vec(vec![
                Value::Nil,
                Value::String(lua.create_string("EINVAL: fs event is not started")?),
                Value::String(lua.create_string("EINVAL")?),
            ]));
        };
        Ok(MultiValue::from_vec(vec![Value::String(
            lua.create_string(path)?,
        )]))
    }

}
impl Drop for LuaFsEvent {
    fn drop(&mut self) {
        self.close_handle();
    }
}

impl UserData for LuaFsEvent {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "start",
            |lua, this, (path, flags, callback): (LuaString, Table, Function)| {
                this.start(lua, path, &flags, callback)
            },
        );
        methods.add_method("stop", |_, this, ()| Ok(this.stop_watching()));
        methods.add_method("getpath", |lua, this, ()| this.getpath(lua));
        methods.add_method("is_closing", |_, this, ()| {
            Ok(this.phase.get() == FsEventPhase::Closed)
        });
        methods.add_method("close", |_, this, ()| {
            this.close_handle();
            Ok(())
        });
    }
}

fn install_fs_event(
    lua: &Lua,
    uv: &Table,
    access: &LoopAccess,
    routes: &Rc<RefCell<HashMap<u64, FsEventRoute>>>,
    next_id: &Rc<Cell<u64>>,
    wake: &ox_uv::AsyncSender,
) -> mlua::Result<()> {
    let event_access = access.clone();
    let event_routes = routes.clone();
    let event_next = next_id.clone();
    let event_wake = wake.clone();
    uv.set(
        "new_fs_event",
        lua.create_function(move |lua, ()| {
            lua.create_userdata(LuaFsEvent {
                state: Rc::new(RefCell::new(None)),
                access: event_access.clone(),
                routes: event_routes.clone(),
                next_id: event_next.clone(),
                phase: Rc::new(Cell::new(FsEventPhase::Idle)),
                wake: event_wake.clone(),
                path: RefCell::new(None),
            })
        })?,
    )?;
    // The module forms luv documents beside `new_fs_event`. Each delegates
    // to the one implementation the method form uses, so lifecycle refusals
    // keep their exact strings on both surfaces.
    uv.set(
        "fs_event_start",
        lua.create_function(
            |lua, (handle, path, flags, callback): (AnyUserData, LuaString, Table, Function)| {
                handle
                    .borrow::<LuaFsEvent>()?
                    .start(lua, path, &flags, callback)
            },
        )?,
    )?;
    uv.set(
        "fs_event_stop",
        lua.create_function(|_, handle: AnyUserData| {
            Ok(handle.borrow::<LuaFsEvent>()?.stop_watching())
        })?,
    )?;
    uv.set(
        "fs_event_getpath",
        lua.create_function(|lua, handle: AnyUserData| {
            handle.borrow::<LuaFsEvent>()?.getpath(lua)
        })?,
    )?;
    Ok(())
}

fn install_aux(
    lua: &Lua,
    uv: &Table,
    access: &LoopAccess,
    fast: &FastCallbackState,
) -> mlua::Result<()> {
    for (name, kind) in [("new_idle", 0_u8), ("new_prepare", 1), ("new_check", 2)] {
        let access = access.clone();
        let lua_for_handle = lua.clone();
        let fast = fast.clone();
        uv.set(
            name,
            lua.create_function(move |lua, ()| {
                let handle = match kind {
                    0 => PhaseHandle::Idle(
                        Idle::new(&mut access.uv_loop.borrow_mut())
                            .map_err(mlua::Error::external)?,
                    ),
                    1 => PhaseHandle::Prepare(
                        Prepare::new(&mut access.uv_loop.borrow_mut())
                            .map_err(mlua::Error::external)?,
                    ),
                    _ => PhaseHandle::Check(
                        Check::new(&mut access.uv_loop.borrow_mut())
                            .map_err(mlua::Error::external)?,
                    ),
                };
                lua.create_userdata(LuaPhase {
                    handle,
                    access: access.clone(),
                    lua: lua_for_handle.clone(),
                    fast: fast.clone(),
                })
            })?,
        )?;
    }
    let async_access = access.clone();
    let async_lua = lua.clone();
    let async_fast = fast.clone();
    uv.set(
        "new_async",
        lua.create_function(move |lua, callback: Function| {
            let event_access = async_access.clone();
            let event_lua = async_lua.clone();
            let event_fast = async_fast.clone();
            let handle = Async::new(&mut async_access.uv_loop.borrow_mut(), move |loop_, _| {
                event_access.callback(loop_, || {
                    invoke(&event_lua, &event_fast, &callback, MultiValue::new());
                });
                Ok(())
            })
            .map_err(mlua::Error::external)?;
            lua.create_userdata(LuaAsync {
                handle,
                access: async_access.clone(),
            })
        })?,
    )?;
    let signal_access = access.clone();
    let signal_lua = lua.clone();
    let signal_fast = fast.clone();
    uv.set(
        "new_signal",
        lua.create_function(move |lua, ()| {
            let handle = Signal::new(&mut signal_access.uv_loop.borrow_mut())
                .map_err(mlua::Error::external)?;
            lua.create_userdata(LuaSignal {
                handle,
                access: signal_access.clone(),
                lua: signal_lua.clone(),
                fast: signal_fast.clone(),
            })
        })?,
    )?;
    uv.set(
        "signal_start",
        lua.create_function(
            |_, (signal, signum, callback): (AnyUserData, Value, Function)| {
                signal.borrow::<LuaSignal>()?.start(signum, callback, false)
            },
        )?,
    )?;
    uv.set(
        "signal_start_oneshot",
        lua.create_function(
            |_, (signal, signum, callback): (AnyUserData, Value, Function)| {
                signal.borrow::<LuaSignal>()?.start(signum, callback, true)
            },
        )?,
    )?;
    uv.set(
        "signal_stop",
        lua.create_function(|_, signal: AnyUserData| Ok(signal.borrow::<LuaSignal>()?.stop()))?,
    )?;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "binds the vim.uv surface as one unit: every registration closure shares the loop access, scheduler, and fast-callback context, so splitting would thread a five-field bundle through artificial helpers"
)]
pub(crate) fn install(
    lua: &Lua,
    uv: &Table,
    access: LoopAccess,
    scheduler: Rc<dyn Scheduler>,
    fast: FastCallbackState,
) -> mlua::Result<()> {
    let uv_loop = access.uv_loop.clone();
    let pending_works: Rc<RefCell<Vec<std::rc::Weak<PendingWork>>>> =
        Rc::new(RefCell::new(Vec::new()));
    let tcp_context = Rc::new(TcpContext {
        lua: lua.clone(),
        fast: fast.clone(),
        access: access.clone(),
        routes: RefCell::new(HashMap::new()),
    });
    let pending_processes: Rc<RefCell<Vec<PendingProcess>>> = Rc::new(RefCell::new(Vec::new()));
    let fs_event_routes: Rc<RefCell<HashMap<u64, FsEventRoute>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let fs_event_next: Rc<Cell<u64>> = Rc::new(Cell::new(0));

    let completion_pending = pending_processes.clone();
    let completion_lua = lua.clone();
    let completion_fast = fast.clone();
    let fs_drain_routes = fs_event_routes.clone();
    let fs_drain_lua = lua.clone();
    let fs_drain_fast = fast.clone();
    access.set_after_run(Rc::new(move || {
        loop {
            let completed = {
                let mut pending = completion_pending.borrow_mut();
                let mut found = None;
                for index in 0..pending.len() {
                    let result = pending[index]
                        .completion
                        .result
                        .lock()
                        .map_err(|_| mlua::Error::runtime("process completion lock poisoned"))?
                        .take();
                    if let Some(result) = result {
                        found = Some((pending.remove(index), result));
                        break;
                    }
                }
                found
            };
            let Some((process, result)) = completed else {
                break;
            };
            let (code, signal) = result.map_err(mlua::Error::runtime)?;
            let mut args = MultiValue::new();
            args.push_back(Value::Integer(code));
            args.push_back(Value::Integer(i64::from(signal)));
            let _guard = completion_fast.enter();
            call_with_traceback(&completion_lua, &process.callback, args)?;
        }
        // Filesystem events drain here rather than in a second hook: the
        // watcher callback is `Send`-confined to plain data, so the queued
        // records are collected first (no borrow is held across the Lua
        // call, keeping reentrant `stop`/`close` panic-free) and then
        // delivered with the same traceback reporting as exits.
        let ready: Vec<(u64, Vec<FsQueueItem>)> = fs_drain_routes
            .borrow()
            .iter()
            .map(|(id, route)| {
                let items = route
                    .queue
                    .lock()
                    .map(|mut pending| Vec::from(std::mem::take(&mut *pending)))
                    .unwrap_or_default();
                (*id, items)
            })
            .collect();
        for (id, items) in ready {
            for event in items {
                let Some((callback, phase)) = fs_drain_routes
                    .borrow()
                    .get(&id)
                    .map(|route| (route.callback.clone(), route.phase.clone()))
                else {
                    break;
                };
                if phase.get() != FsEventPhase::Active(id) {
                    break;
                }
                // A callback is user code that can stop or close its own
                // handle, so every delivery rechecks the live route state
                // instead of trusting the snapshot above. `teardown` lands
                // the phase transition even when it must skip the route
                // removal, so both checks must pass; neither borrow
                // survives into the callback.
                let mut args = MultiValue::new();
                match event {
                    Ok((filename, change, rename)) => {
                        args.push_back(Value::Nil);
                        args.push_back(Value::String(fs_drain_lua.create_string(&filename)?));
                        let events = fs_drain_lua.create_table()?;
                        events.set("change", change)?;
                        events.set("rename", rename)?;
                        args.push_back(Value::Table(events));
                    }
                    Err(message) => {
                        args.push_back(Value::String(fs_drain_lua.create_string(message)?));
                        args.push_back(Value::Nil);
                        args.push_back(Value::Nil);
                    }
                }
                let _guard = fs_drain_fast.enter();
                call_with_traceback(&fs_drain_lua, &callback, args)?;
            }
        }
        Ok(())
    }));

    let run_access = access.clone();
    let run_works = pending_works.clone();
    let run_scheduler = scheduler;
    let run_lua = lua.clone();
    let run_fast = fast.clone();
    uv.set(
        "run",
        lua.create_function(move |_, mode: Option<String>| {
            let mode = match mode.as_deref().unwrap_or("default") {
                "default" => RunMode::Default,
                "once" => RunMode::Once,
                "nowait" => RunMode::NoWait,
                other => return Err(mlua::Error::runtime(format!("invalid run mode: {other}"))),
            };
            let alive = run_access.run(mode)?;
            let works: Vec<_> = run_works
                .borrow_mut()
                .drain(..)
                .filter_map(|pending| pending.upgrade())
                .collect();
            for pending in &works {
                loop {
                    let result = pending
                        .completion
                        .results
                        .lock()
                        .map_err(|_| mlua::Error::runtime("work completion lock poisoned"))?
                        .pop_front();
                    let Some(result) = result else { break };
                    let callback = pending.callback.clone();
                    let lua = run_lua.clone();
                    let fast = run_fast.clone();
                    run_scheduler
                        .schedule_deferred(Box::new(move || {
                            let mut args = MultiValue::new();
                            match result {
                                Ok(values) => {
                                    args.push_back(Value::Nil);
                                    for value in values {
                                        push_thread_arg(&lua, &mut args, value)
                                            .map_err(mlua::Error::runtime)?;
                                    }
                                }
                                Err(error) => {
                                    args.push_back(Value::String(lua.create_string(error)?));
                                }
                            }
                            let _guard = fast.enter();
                            call_with_traceback(&lua, &callback, args).map(|_| ())
                        }))
                        .map_err(mlua::Error::runtime)?;
                }
            }
            let live = works.iter().map(Rc::downgrade).collect::<Vec<_>>();
            *run_works.borrow_mut() = live;
            Ok(alive)
        })?,
    )?;

    let new_tcp_context = tcp_context.clone();
    uv.set(
        "new_tcp",
        lua.create_function(move |lua, ()| {
            lua.create_userdata(LuaTcp {
                inner: Rc::new(RefCell::new(None)),
                callbacks: Rc::new(RefCell::new(StreamCallbacks::default())),
                context: new_tcp_context.clone(),
                closing: Rc::new(Cell::new(false)),
            })
        })?,
    )?;

    #[cfg(unix)]
    {
        let pipe_access = access.clone();
        let pipe_fast = fast.clone();
        let pipe_lua = lua.clone();
        uv.set(
            "new_pipe",
            lua.create_function(move |lua, _ipc: Option<bool>| {
                lua.create_userdata(LuaProcessPipe {
                    inner: Rc::new(RefCell::new(None)),
                    callbacks: Rc::new(RefCell::new(StreamCallbacks::default())),
                    lua: pipe_lua.clone(),
                    fast: pipe_fast.clone(),
                    access: pipe_access.clone(),
                    closing: Rc::new(Cell::new(false)),
                })
            })?,
        )?;

        let spawn_loop = uv_loop.clone();
        let spawn_pending = pending_processes.clone();
        let spawn_access = access.clone();
        uv.set("spawn", lua.create_function(move |lua, (program, options, callback): (String, Table, Function)| {
            let mut spawn_options = SpawnOptions::new(program);
            if let Some(args) = options.get::<Option<Table>>("args")? {
                spawn_options.args = args
                    .sequence_values::<String>()
                    .map(|argument| argument.map(OsString::from))
                    .collect::<mlua::Result<Vec<_>>>()?;
            }
            if let Some(environment) = options.get::<Option<Table>>("env")? {
                spawn_options.environment = Some(
                    environment
                        .sequence_values::<String>()
                        .map(|entry| {
                            let entry = entry?;
                            let Some((name, value)) = entry.split_once('=') else {
                                return Err(mlua::Error::runtime("environment entries must have the form NAME=VALUE"));
                            };
                            Ok((OsString::from(name), OsString::from(value)))
                        })
                        .collect::<mlua::Result<Vec<_>>>()?,
                );
            }
            spawn_options.cwd = options.get::<Option<String>>("cwd")?.map(PathBuf::from);
            spawn_options.detached = options.get::<Option<bool>>("detached")?.unwrap_or(false);
            spawn_options.uid = options.get::<Option<u32>>("uid")?;
            spawn_options.gid = options.get::<Option<u32>>("gid")?;
            let stdio = options.get::<Option<Table>>("stdio")?;
            let mut pipe_targets: [Option<LuaProcessPipe>; 3] = [None, None, None];
            if let Some(stdio) = stdio {
                for (index, target) in pipe_targets.iter_mut().enumerate() {
                    match stdio.raw_get::<Value>(index + 1)? {
                        Value::UserData(userdata) => {
                            let pipe = userdata.borrow::<LuaProcessPipe>()?.clone();
                            spawn_options.stdio[index] = StdioConfig::CreatePipe;
                            *target = Some(pipe);
                        }
                        Value::Nil | Value::Boolean(false) => {
                            spawn_options.stdio[index] = StdioConfig::Ignore;
                        }
                        Value::Integer(fd @ 0..=2) => {
                            spawn_options.stdio[index] = StdioConfig::InheritFd(
                                u8::try_from(fd).map_err(mlua::Error::external)?,
                            );
                        }
                        _ => {
                            return Err(mlua::Error::runtime(
                                "stdio entries must be pipe handles, false, nil, or descriptors 0, 1, or 2",
                            ));
                        }
                    }
                }
            }
            let completion = Arc::new(ProcessCompletion::default()); let waiter = completion.clone();
            let spawned = process::spawn(&mut spawn_loop.borrow_mut(), spawn_options, move |_, result| { let value = result.map(|exit| (exit.code, exit.signal)).map_err(|error| error.to_string()); if let Ok(mut slot) = waiter.result.lock() { *slot = Some(value); } }).map_err(mlua::Error::external)?;
            let mut pipes = spawned.pipes;
            if let (Some(target), Some(pipe)) = (&pipe_targets[0], pipes.stdin.take()) { target.install_endpoint(pipe); }
            if let (Some(target), Some(pipe)) = (&pipe_targets[1], pipes.stdout.take()) { target.install_endpoint(pipe); }
            if let (Some(target), Some(pipe)) = (&pipe_targets[2], pipes.stderr.take()) { target.install_endpoint(pipe); }
            let process = LuaProcess { inner: Rc::new(RefCell::new(Some(spawned.process))), access: spawn_access.clone(), closing: Rc::new(Cell::new(false)) };
            let pid = process.inner.borrow().as_ref().map(Process::pid).unwrap_or_default();
            spawn_pending.borrow_mut().push(PendingProcess { completion, callback });
            Ok((lua.create_userdata(process)?, pid))
        })?)?;
    }

    uv.set(
        "new_thread",
        lua.create_function(move |lua, (function, args): (Function, Variadic<Value>)| {
            let chunk = function.dump(false);
            let arguments = args
                .into_iter()
                .map(thread_arg)
                .collect::<mlua::Result<Vec<_>>>()?;
            let thread =
                thread::new_thread(None, move |_| run_isolated(&chunk, arguments).map(|_| ()))
                    .map_err(mlua::Error::external)?;
            lua.create_userdata(LuaThread {
                inner: RefCell::new(Some(thread)),
            })
        })?,
    )?;

    let work_pending = pending_works.clone();
    let work_loop = uv_loop.clone();
    uv.set(
        "new_work",
        lua.create_function(
            move |lua, (work_function, after_function): (Function, Function)| {
                let chunk = work_function.dump(false);
                let completion = Arc::new(WorkCompletion::default());
                let after_completion = completion.clone();
                let pool = ox_uv::pool::Pool::new();
                let poster = work_loop.borrow().completion_poster();
                let work = ox_uv::work::new_work(
                    pool,
                    poster,
                    move |data| {
                        let arguments = data
                            .downcast::<Vec<ThreadArg>>()
                            .map(|data| *data)
                            .unwrap_or_default();
                        Box::new(run_isolated(&chunk, arguments)) as ox_uv::work::WorkData
                    },
                    move |_, result| {
                        let value = result.map_err(|error| error.to_string()).and_then(|data| {
                            data.downcast::<Result<Vec<ThreadArg>, String>>()
                                .map_or_else(|_| Err("invalid work result".into()), |data| *data)
                        });
                        if let Ok(mut results) = after_completion.results.lock() {
                            results.push_back(value);
                        }
                    },
                );
                let pending = Rc::new(PendingWork {
                    completion,
                    callback: after_function,
                });
                work_pending.borrow_mut().push(Rc::downgrade(&pending));
                lua.create_userdata(LuaWork { work, pending })
            },
        )?,
    )?;

    let addrinfo_fast = fast.clone();
    uv.set("getaddrinfo", lua.create_function(move |lua, (host, service, callback): (Option<String>, Option<String>, Option<Function>)| {
        let result = dns::getaddrinfo(host.as_deref(), service.as_deref(), AddrInfoHints::default());
        let values = match result { Ok(entries) => { let table = lua.create_table()?; for (index, entry) in entries.into_iter().enumerate() { let item = address_table(lua, SocketAddr::new(entry.address, entry.port))?; item.set("socktype", format!("{:?}", entry.socket_type).to_lowercase())?; table.raw_set(index + 1, item)?; } Value::Table(table) }, Err(error) => { if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::String(lua.create_string(error.to_string())?)); invoke(lua, &addrinfo_fast, &callback, args); return Ok(Value::Nil); } return Err(mlua::Error::external(error)); } };
        if let Some(callback) = callback { let mut args = MultiValue::new(); args.push_back(Value::Nil); args.push_back(values); invoke(lua, &addrinfo_fast, &callback, args); Ok(Value::Nil) } else { Ok(values) }
    })?)?;
    let nameinfo_fast = fast.clone();
    uv.set(
        "getnameinfo",
        lua.create_function(
            move |lua, (host, port, callback): (String, u16, Option<Function>)| {
                let info =
                    dns::getnameinfo(socket_addr(&host, port)?).map_err(mlua::Error::external)?;
                if let Some(callback) = callback {
                    let mut args = MultiValue::new();
                    args.push_back(Value::Nil);
                    args.push_back(Value::String(lua.create_string(info.host)?));
                    args.push_back(Value::String(lua.create_string(info.service)?));
                    invoke(lua, &nameinfo_fast, &callback, args);
                    Ok(Value::Nil)
                } else {
                    let table = lua.create_table()?;
                    table.set("host", info.host)?;
                    table.set("service", info.service)?;
                    Ok(Value::Table(table))
                }
            },
        )?,
    )?;

    install_aux(lua, uv, &access, &fast)?;

    // Allocate an always-live async handle to wake the loop when a watcher
    // thread posts an event. The handle is unreferenced so it never keeps
    // a default run alive by itself; its callback drains the fs-event routes
    // through the same after_run closure used at run-return time.
    let fs_event_wake = Async::new(&mut access.uv_loop.borrow_mut(), {
        let wake_access = access.clone();
        move |loop_, _| {
            // Drain inside the callback frame so queued fs events reach their
            // Lua callbacks during an active default run instead of waiting
            // for run-return; drain errors surface through the callback-error
            // channel like every other handle callback failure.
            wake_access.callback(loop_, || {
                wake_access
                    .finish_run()
                    .map_err(|error| CallbackError::new(error.to_string()))
            })
        }
    })
    .map_err(mlua::Error::external)?;
    fs_event_wake
        .unref(&mut access.uv_loop.borrow_mut())
        .map_err(mlua::Error::external)?;
    let fs_event_sender = fs_event_wake
        .sender(&access.uv_loop.borrow())
        .map_err(mlua::Error::external)?;
    install_fs_event(
        lua,
        uv,
        &access,
        &fs_event_routes,
        &fs_event_next,
        &fs_event_sender,
    )?;
    install_udp_tty(lua, uv, access, fast)?;
    Ok(())
}

#[derive(Default)]
struct UdpCallbacks {
    recv: Option<Function>,
    sends: HashMap<u64, Function>,
}
#[derive(Clone)]
struct LuaUdp {
    inner: Rc<RefCell<Option<Udp>>>,
    callbacks: Rc<RefCell<UdpCallbacks>>,
    lua: Lua,
    fast: FastCallbackState,
    access: LoopAccess,
    closing: Rc<Cell<bool>>,
}
impl Drop for LuaUdp {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) != 1 || self.closing.replace(true) {
            return;
        }
        let inner = self.inner.clone();
        let callbacks = self.callbacks.clone();
        self.access.apply(Box::new(move |uv_loop| {
            if let Some(udp) = inner.borrow_mut().take() {
                let _ = udp.close(uv_loop);
                *callbacks.borrow_mut() = UdpCallbacks::default();
            }
        }));
    }
}
impl UserData for LuaUdp {
    #[expect(
        clippy::too_many_lines,
        reason = "UDP method closures share the recv/send callback table; registration is one dispatch unit"
    )]
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bind", |_, this, (host, port): (String, u16)| {
            let address = socket_addr(&host, port)?;
            let callbacks = this.callbacks.clone();
            let lua = this.lua.clone();
            let fast = this.fast.clone();
            let access = this.access.clone();
            let event_access = access.clone();
            let udp = Udp::bind(
                &mut access.uv_loop.borrow_mut(),
                address,
                move |loop_, _, event| {
                    event_access.callback(loop_, || match event {
                        NetEvent::Datagram { data, from } => {
                            let callback = callbacks.borrow().recv.clone();
                            if let Some(callback) = callback
                                && let (Ok(data), Ok(address)) =
                                    (lua.create_string(data), address_table(&lua, from))
                            {
                                let mut args = MultiValue::new();
                                args.push_back(Value::Nil);
                                args.push_back(Value::String(data));
                                args.push_back(Value::Table(address));
                                invoke(&lua, &fast, &callback, args);
                            }
                        }
                        NetEvent::WriteComplete { id, result } => {
                            let callback = callbacks.borrow_mut().sends.remove(&id.get());
                            if let Some(callback) = callback
                                && let Ok(args) = error_args(&lua, result)
                            {
                                invoke(&lua, &fast, &callback, args);
                            }
                        }
                        NetEvent::Error(error) => {
                            let callback = callbacks.borrow().recv.clone();
                            if let Some(callback) = callback {
                                let mut args = MultiValue::new();
                                if let Ok(message) = lua.create_string(error.to_string()) {
                                    args.push_back(Value::String(message));
                                    args.push_back(Value::Nil);
                                    invoke(&lua, &fast, &callback, args);
                                }
                            }
                        }
                        _ => {}
                    });
                },
            )
            .map_err(mlua::Error::external)?;
            *this.inner.borrow_mut() = Some(udp);
            Ok(true)
        });
        methods.add_method("connect", |_, this, (host, port): (String, u16)| {
            let address = socket_addr(&host, port)?;
            this.inner
                .borrow()
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?
                .connect(address)
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("recv_start", |_, this, callback: Function| {
            this.callbacks.borrow_mut().recv = Some(callback);
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(udp) = inner.borrow_mut().as_mut() {
                    let _ = udp.recv_start(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method("recv_stop", |_, this, ()| {
            this.callbacks.borrow_mut().recv = None;
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(udp) = inner.borrow_mut().as_mut() {
                    let _ = udp.recv_stop(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method(
            "send",
            |_,
             this,
             (bytes, host, port, callback): (
                LuaString,
                Option<String>,
                Option<u16>,
                Option<Function>,
            )| {
                let target = match (host, port) {
                    (Some(host), Some(port)) => Some(socket_addr(&host, port)?),
                    (None, None) => None,
                    _ => return Err(mlua::Error::runtime("UDP target needs host and port")),
                };
                let data = bytes.as_bytes().to_vec();
                let inner = this.inner.clone();
                let callbacks = this.callbacks.clone();
                this.access.apply(Box::new(move |uv_loop| {
                    if let Some(udp) = inner.borrow_mut().as_mut()
                        && let Ok(id) = udp.send(uv_loop, data, target)
                        && let Some(callback) = callback
                    {
                        callbacks.borrow_mut().sends.insert(id.get(), callback);
                    }
                }));
                Ok(true)
            },
        );
        methods.add_method("getsockname", |lua, this, ()| {
            let inner = this.inner.borrow();
            address_table(
                lua,
                inner
                    .as_ref()
                    .ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?
                    .local_addr()
                    .map_err(mlua::Error::external)?,
            )
        });
        methods.add_method("getpeername", |lua, this, ()| {
            let inner = this.inner.borrow();
            address_table(
                lua,
                inner
                    .as_ref()
                    .ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?
                    .peer_addr()
                    .map_err(mlua::Error::external)?,
            )
        });
        methods.add_method("set_broadcast", |_, this, enable: bool| {
            this.inner
                .borrow()
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?
                .set_broadcast(enable)
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("set_ttl", |_, this, ttl: u32| {
            this.inner
                .borrow()
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("UDP handle is not initialized"))?
                .set_ttl(ttl)
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("close", |_, this, ()| {
            if this.closing.replace(true) {
                return Ok(());
            }
            let inner = this.inner.clone();
            let callbacks = this.callbacks.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(udp) = inner.borrow_mut().take() {
                    let _ = udp.close(uv_loop);
                    *callbacks.borrow_mut() = UdpCallbacks::default();
                }
            }));
            Ok(())
        });
    }
}

fn install_udp_tty(
    lua: &Lua,
    uv: &Table,
    access: LoopAccess,
    fast: FastCallbackState,
) -> mlua::Result<()> {
    #[cfg(unix)]
    let (tty_access, tty_lua, tty_fast) = (access.clone(), lua.clone(), fast.clone());
    let udp_access = access;
    let udp_fast = fast;
    let udp_lua = lua.clone();
    uv.set(
        "new_udp",
        lua.create_function(move |lua, ()| {
            lua.create_userdata(LuaUdp {
                inner: Rc::new(RefCell::new(None)),
                callbacks: Rc::new(RefCell::new(UdpCallbacks::default())),
                lua: udp_lua.clone(),
                fast: udp_fast.clone(),
                access: udp_access.clone(),
                closing: Rc::new(Cell::new(false)),
            })
        })?,
    )?;
    #[cfg(unix)]
    {
        uv.set(
            "new_tty",
            lua.create_function(move |lua, (fd, readable): (i32, bool)| {
                let path = format!("/proc/self/fd/{fd}");
                let file = OpenOptions::new()
                    .read(readable)
                    .write(!readable)
                    .open(path)
                    .map_err(mlua::Error::external)?;
                let callbacks = Rc::new(RefCell::new(StreamCallbacks::default()));
                let event_callbacks = callbacks.clone();
                let event_lua = tty_lua.clone();
                let event_fast = tty_fast.clone();
                let event_access = tty_access.clone();
                let tty = Tty::open(
                    &mut tty_access.uv_loop.borrow_mut(),
                    file,
                    readable,
                    move |loop_, _, event| {
                        event_access.callback(loop_, || match event {
                            NetEvent::Read(bytes) => {
                                let callback = event_callbacks.borrow().read.clone();
                                if let Some(callback) = callback
                                    && let Ok(bytes) = event_lua.create_string(bytes)
                                {
                                    let mut args = MultiValue::new();
                                    args.push_back(Value::Nil);
                                    args.push_back(Value::String(bytes));
                                    invoke(&event_lua, &event_fast, &callback, args);
                                }
                            }
                            NetEvent::Eof => {
                                let callback = event_callbacks.borrow().read.clone();
                                if let Some(callback) = callback {
                                    let mut args = MultiValue::new();
                                    args.push_back(Value::Nil);
                                    args.push_back(Value::Nil);
                                    invoke(&event_lua, &event_fast, &callback, args);
                                }
                            }
                            NetEvent::WriteComplete { id, result } => {
                                let callback =
                                    event_callbacks.borrow_mut().writes.remove(&id.get());
                                if let Some(callback) = callback
                                    && let Ok(args) = error_args(&event_lua, result)
                                {
                                    invoke(&event_lua, &event_fast, &callback, args);
                                }
                            }
                            _ => {}
                        });
                    },
                )
                .map_err(mlua::Error::external)?;
                lua.create_userdata(LuaTty {
                    inner: Rc::new(RefCell::new(Some(tty))),
                    callbacks,
                    access: tty_access.clone(),
                    closing: Rc::new(Cell::new(false)),
                })
            })?,
        )?;
    }
    Ok(())
}

#[cfg(unix)]
struct LuaTty {
    inner: Rc<RefCell<Option<Tty>>>,
    callbacks: Rc<RefCell<StreamCallbacks>>,
    access: LoopAccess,
    closing: Rc<Cell<bool>>,
}
impl Drop for LuaTty {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) != 1 || self.closing.replace(true) {
            return;
        }
        let inner = self.inner.clone();
        let callbacks = self.callbacks.clone();
        self.access.apply(Box::new(move |uv_loop| {
            if let Some(tty) = inner.borrow_mut().take() {
                let _ = tty.close(uv_loop);
                *callbacks.borrow_mut() = StreamCallbacks::default();
            }
        }));
    }
}
#[cfg(unix)]
impl UserData for LuaTty {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("read_start", |_, this, callback: Function| {
            this.callbacks.borrow_mut().read = Some(callback);
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(tty) = inner.borrow_mut().as_mut() {
                    let _ = tty.read_start(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method("read_stop", |_, this, ()| {
            this.callbacks.borrow_mut().read = None;
            let inner = this.inner.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(tty) = inner.borrow_mut().as_mut() {
                    let _ = tty.read_stop(uv_loop);
                }
            }));
            Ok(true)
        });
        methods.add_method(
            "write",
            |_, this, (bytes, callback): (LuaString, Option<Function>)| {
                let data = bytes.as_bytes().to_vec();
                let inner = this.inner.clone();
                let callbacks = this.callbacks.clone();
                this.access.apply(Box::new(move |uv_loop| {
                    if let Some(tty) = inner.borrow_mut().as_mut()
                        && let Ok(id) = tty.write(uv_loop, data)
                        && let Some(callback) = callback
                    {
                        callbacks.borrow_mut().writes.insert(id.get(), callback);
                    }
                }));
                Ok(true)
            },
        );
        methods.add_method("get_winsize", |_, this, ()| {
            this.inner
                .borrow()
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("TTY is closed"))?
                .get_winsize()
                .map_err(mlua::Error::external)
        });
        methods.add_method("set_mode", |_, this, mode: String| {
            let mode = match mode.as_str() {
                "normal" => TtyMode::Normal,
                "raw" => TtyMode::Raw,
                "io" => TtyMode::Cbreak,
                _ => return Err(mlua::Error::runtime("invalid TTY mode")),
            };
            this.inner
                .borrow()
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("TTY is closed"))?
                .set_mode(mode)
                .map_err(mlua::Error::external)?;
            Ok(true)
        });
        methods.add_method("close", |_, this, ()| {
            if this.closing.replace(true) {
                return Ok(());
            }
            let inner = this.inner.clone();
            let callbacks = this.callbacks.clone();
            this.access.apply(Box::new(move |uv_loop| {
                if let Some(tty) = inner.borrow_mut().take() {
                    let _ = tty.close(uv_loop);
                    *callbacks.borrow_mut() = StreamCallbacks::default();
                }
            }));
            Ok(())
        });
    }
}

#[cfg(test)]
mod loop_access_tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::panic,
        reason = "the unwind test must construct a loop, assert the panic, and panic once itself"
    )]
    use super::*;

    #[test]
    fn loop_state_restored_after_callback_panic() {
        let access = LoopAccess::new(Rc::new(RefCell::new(
            UvLoop::new().expect("test loop must construct"),
        )));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            access.callback(&mut access.uv_loop.borrow_mut(), || {
                panic!("sentinel callback panic");
            });
        }))
        .expect_err("the panicking callback must propagate its panic");
        assert_eq!(
            panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str)),
            Some("sentinel callback panic")
        );
        assert!(
            !access.in_callback.get(),
            "in_callback must unwind to false"
        );
        assert!(
            access.active_loop.get().is_none(),
            "active_loop must unwind to None"
        );
        assert!(!access.draining.get(), "draining must unwind to false");
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "the lifecycle tests construct a host, drive Lua, and panic on assertion failure"
)]
mod fs_event_lifecycle_tests {
    use super::LuaFsEvent;
    use crate::{BuiltinHost, LuaHost, RuntimeRoot, Scheduler, Work};
    use mlua::AnyUserData;
    use ox_types::{OxStr, Typval};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::rc::Rc;


    struct TestScheduler {
        queue: RefCell<VecDeque<Work>>,
    }

    impl Scheduler for TestScheduler {
        fn schedule_deferred(&self, work: Work) -> Result<(), String> {
            self.queue.borrow_mut().push_back(work);
            Ok(())
        }
    }

    struct TestBuiltins;

    impl BuiltinHost for TestBuiltins {
        fn call(&self, name: &OxStr, _args: Vec<Typval>) -> Result<Typval, String> {
            // The runtime prelude probes has('win32') during host init.
            if name.as_bytes() == b"has" {
                return Ok(Typval::Number(0));
            }
            Err(format!("unexpected Vimscript call: {name:?}"))
        }
    }

    /// Removes the watched tree on drop, even after an assertion failure.
    struct TempWatchDir {
        path: PathBuf,
    }

    impl TempWatchDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "oxvim-uv-handles-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create watch dir");
            Self { path }
        }
    }

    impl Drop for TempWatchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn host() -> LuaHost {
        let scheduler = Rc::new(TestScheduler {
            queue: RefCell::new(VecDeque::new()),
        });
        let runtime = RuntimeRoot::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime"),
        );
        LuaHost::new(runtime, Rc::new(TestBuiltins), scheduler).expect("create host")
    }

    fn with_watch_dir(label: &str, script: &str) {
        let dir = TempWatchDir::new(label);
        let host = host();
        host.lua()
            .globals()
            .set("TEST_DIR", dir.path.to_string_lossy().as_ref())
            .unwrap();
        host.lua().load(script).exec().unwrap();
    }

    /// F11 + F12 + F50: `close` then `start` refuses and never resurrects;
    /// `stop` returns to a restartable state on the same userdata.
    #[test]
    fn fs_event_handle_lifecycle_refuses_closed_and_allows_restart() {
        with_watch_dir(
            "lifecycle",
            r"
            local closed_handle = assert(vim.uv.new_fs_event())
            closed_handle:close()
            assert(closed_handle:is_closing())
            local ok, err = pcall(closed_handle.start, closed_handle, TEST_DIR, {}, function() end)
            assert(not ok, 'start after close must refuse')
            assert(tostring(err):find('fs event is closed', 1, true), err)
            assert(not pcall(closed_handle.start, closed_handle, TEST_DIR, {}, function() end),
              'a closed handle must never resurrect')

            local handle = assert(vim.uv.new_fs_event())
            assert(handle:start(TEST_DIR, {}, function() end) == 0)
            assert(handle:stop() == 0)
            assert(not handle:is_closing())
            assert(handle:start(TEST_DIR, {}, function() end) == 0,
              'stop must allow restarting the same handle')
            assert(handle:stop() == 0)
            handle:close()
            assert(handle:is_closing())
            ",
        );
    }

    /// F20: two racing `start` calls reserve the handle state synchronously,
    /// so exactly one watcher starts and `stop` still controls it.
    #[test]
    fn fs_event_racing_starts_refuse_reentry() {
        with_watch_dir(
            "racing",
            r"
            local handle = assert(vim.uv.new_fs_event())
            local raced = false
            local async = assert(vim.uv.new_async(function()
              assert(handle:start(TEST_DIR, {}, function() end) == 0)
              local ok, err = pcall(handle.start, handle, TEST_DIR, {}, function() end)
              assert(not ok, 'second racing start must refuse')
              assert(tostring(err):find('already started', 1, true), err)
              raced = true
            end))
            async:send()
            vim.uv.run('nowait')
            assert(raced, 'async callback must have run')
            assert(handle:stop() == 0, 'the queued start must yield a watcher stop controls')
            handle:close()
            ",
        );
    }

    /// F22: a queued event reaches its Lua callback during an active default
    /// run, and the callback's own close is what ends the run.
    #[test]
    fn fs_event_delivers_queued_events_during_default_run() {
        let dir = TempWatchDir::new("delivery");
        let host = host();
        host.lua()
            .globals()
            .set("TEST_DIR", dir.path.to_string_lossy().as_ref())
            .unwrap();
        host.lua()
            .load(
                r"
                handle = assert(vim.uv.new_fs_event())
                delivered = false
                assert(handle:start(TEST_DIR, {}, function()
                  delivered = true
                  handle:close()
                end) == 0)
                ",
            )
            .exec()
            .unwrap();
        std::fs::write(dir.path.join("created.txt"), b"hello").expect("create file");
        host.lua()
            .load(
                r"
                vim.uv.run('default')
                assert(delivered, 'event must reach its callback during the default run')
                assert(handle:is_closing(), 'the delivery callback must have closed the watcher')
                ",
            )
            .exec()
            .unwrap();
    }

    /// F21: Lua-collected userdata must release the loop, not leak. GC runs
    /// `Drop`, whose teardown is the same one `stop`/`close` use (route
    /// removal + `FsEvent::close`); those paths are delivery-observable, so
    /// this test pins the remaining question: that `Drop` actually fires and
    /// the dropped watcher's handle-registry entry stops keeping
    /// `run("default")` alive. A leaked watcher would spin the default run
    /// until the unreferenced guard timer kills it at 3s.
    #[test]
    fn fs_event_collected_userdata_releases_route_and_handle() {
        with_watch_dir(
            "collected",
            r"
            local leaked = assert(vim.uv.new_fs_event())
            assert(leaked:start(TEST_DIR, {}, function() end) == 0)
            leaked = nil
            collectgarbage()
            collectgarbage()
            local guard_fired = false
            local guard = assert(vim.uv.new_timer())
            guard:start(3000, 0, function()
              guard_fired = true
              vim.uv.stop()
            end)
            guard:unref()
            vim.uv.run('default')
            assert(not guard_fired,
              'collected watcher kept the loop alive: route/handle leak')
            guard:close()
            vim.uv.run('nowait')
            ",
        );
    }
    /// A failed startup probe keeps luv's exact ENOENT text: `vim._watch`
    /// notifies on that name and existing tests pin the message.
    #[test]
    fn fs_event_start_reports_missing_path_enoent() {
        with_watch_dir(
            "errno-missing",
            r"
            local handle = assert(vim.uv.new_fs_event())
            local ok, err, name = handle:start(TEST_DIR .. '/absent', {}, function() end)
            assert(ok == nil and name == 'ENOENT', tostring(err))
            assert(
                err == 'ENOENT: no such file or directory: ' .. TEST_DIR .. '/absent',
                err
            )
            handle:close()
            ",
        );
    }

    /// A path behind a mode-000 directory reports EACCES — the errno plugins
    /// branch on — instead of a collapsed ENOENT. Root bypasses mode bits, so
    /// the case is skipped where the probe stat succeeds; asserting there
    /// would prove nothing.
    #[cfg(unix)]
    #[test]
    fn fs_event_start_reports_sealed_path_eacces() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempWatchDir::new("errno-sealed");
        let host = host();
        let sealed = dir.path.join("sealed");
        std::fs::create_dir(&sealed).expect("create sealed dir");
        std::fs::write(sealed.join("probe"), b"probe").expect("create probe");
        // The probe targets a path *under* the sealed directory: stat needs
        // no permission on the final component, so statting the sealed
        // directory itself would succeed even where mode bits bind.
        host.lua()
            .globals()
            .set(
                "TEST_SEALED",
                sealed.join("probe").to_string_lossy().as_ref(),
            )
            .unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0))
            .expect("seal directory");
        if std::fs::metadata(sealed.join("probe")).is_ok() {
            std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755))
                .expect("unseal directory");
            return;
        }
        let result = host.lua()
            .load(
                r"
                local handle = assert(vim.uv.new_fs_event())
                local ok, err, name = handle:start(TEST_SEALED, {}, function() end)
                assert(ok == nil and name == 'EACCES', tostring(err))
                assert(err:sub(1, 8) == 'EACCES: ', err)
                handle:close()
                ",
            )
            .exec();
        // Unseal before any unwinding: the recursive cleanup cannot open a
        // sealed directory.
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755))
            .expect("unseal directory");
        result.unwrap();
    }

    /// P1: luv's module forms route through the same backend as the method
    /// form, so lifecycle refusals keep their exact strings, and `getpath`
    /// answers on both surfaces. `stop` keeps the monitored path, like luv.
    #[test]
    fn fs_event_module_forms_share_backend_with_method_form() {
        with_watch_dir(
            "module-forms",
            r"
            local handle = assert(vim.uv.new_fs_event())
            local ok, err = vim.uv.fs_event_getpath(handle)
            assert(ok == nil and err:find('EINVAL', 1, true), tostring(err))

            assert(vim.uv.fs_event_start(handle, TEST_DIR, {}, function() end) == 0)
            assert(vim.uv.fs_event_getpath(handle) == TEST_DIR)
            assert(select('#', handle:getpath()) == 1)
            assert(handle:getpath() == TEST_DIR)
            assert(vim.uv.fs_event_stop(handle) == 0)
            assert(handle:getpath() == TEST_DIR)

            assert(vim.uv.fs_event_start(handle, TEST_DIR, {}, function() end) == 0)
            local started_ok, started_err =
              pcall(vim.uv.fs_event_start, handle, TEST_DIR, {}, function() end)
            assert(not started_ok, 'module-form start on a started handle must refuse')
            assert(tostring(started_err):find('already started', 1, true), started_err)

            handle:close()
            local closed_ok, closed_err =
              pcall(vim.uv.fs_event_start, handle, TEST_DIR, {}, function() end)
            assert(not closed_ok, 'module-form start on a closed handle must refuse')
            assert(tostring(closed_err):find('fs event is closed', 1, true), closed_err)
            local path_ok, path_err = pcall(vim.uv.fs_event_getpath, handle)
            assert(not path_ok, 'getpath on a closed handle must refuse')
            assert(tostring(path_err):find('fs event is closed', 1, true), path_err)
            vim.uv.run('nowait')
            ",
        );
    }

    /// P2: a callback is user code that can stop its own handle, so the
    /// drain must recheck the route and phase before every delivery. The
    /// two records are seeded into the live route queue from the test side,
    /// so the drain snapshot holds them as one batch by construction
    /// instead of by watcher-thread timing.
    #[test]
    fn fs_event_stop_in_callback_silences_rest_of_batch() {
        let dir = TempWatchDir::new("stop-mid-batch");
        let host = host();
        host.lua()
            .globals()
            .set("TEST_DIR", dir.path.to_string_lossy().as_ref())
            .unwrap();
        host.lua()
            .load(
                r"
                handle = assert(vim.uv.new_fs_event())
                delivered = {}
                assert(handle:start(TEST_DIR, {}, function(_, filename)
                  delivered[#delivered + 1] = filename
                  handle:stop()
                end) == 0)
                -- Land the deferred backend start so the route phase is Active.
                vim.uv.run('nowait')
                ",
            )
            .exec()
            .unwrap();
        let handle = host
            .lua()
            .globals()
            .get::<AnyUserData>("handle")
            .expect("handle global");
        {
            let watcher = handle.borrow::<LuaFsEvent>().expect("fs event userdata");
            let routes = watcher.routes.borrow_mut();
            assert_eq!(routes.len(), 1, "the started handle owns one route");
            for route in routes.values() {
                let mut queue = route.queue.lock().expect("route queue");
                queue.push_back(Ok(("a.txt".into(), false, true)));
                queue.push_back(Ok(("b.txt".into(), true, false)));
            }
        }
        host.lua()
            .load(
                r"
                vim.uv.run('nowait')
                assert(#delivered >= 1, 'the first event must reach its callback')
                assert(#delivered == 1,
                  'events after the callback stopped the handle must never enter plugin code')
                assert(delivered[1] == 'a.txt', tostring(delivered[1]))
                assert(not handle:is_closing(), 'stop leaves the handle restartable, not closed')
                handle:close()
                ",
            )
            .exec()
            .unwrap();
    }
}
