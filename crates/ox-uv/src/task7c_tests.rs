//! Task 7c tests: poll handles, IPC write2/pending, extra stdio, vectored fs,
//! and misc surface. Each test family cites the `runtime/doc/luvref.txt`
//! sections it exercises.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{Handle, UvLoop};
use crate::net::{NetEvent, Pipe, PipeHandleKind};
use crate::process::{self, ExtraStdio, SpawnOptions, StdioConfig};

const TIMEOUT: Duration = Duration::from_secs(10);
static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(label: &str) -> std::path::PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("ox-uv-7c-{label}-{}-{n}", std::process::id()))
}

// ---------------------------------------------------------------------------
// Poll handle: uv_poll_t (luvref 1115-1210).
// ---------------------------------------------------------------------------

/// Watches a pipe read-end fd with a Poll handle; writing to the write end
/// fires the callback with the READABLE event.
#[test]
fn poll_watches_pipe_and_fires_readable() {
    use crate::Poll;
    use crate::poll::PollEvents;

    let (read, write) = rustix::pipe::pipe().expect("create pipe");
    let mut uv_loop = UvLoop::new().expect("create loop");

    let fired: Rc<Cell<Option<PollEvents>>> = Rc::new(Cell::new(None));
    let done = Rc::new(Cell::new(false));

    let poll: Rc<std::cell::RefCell<Option<Poll>>> = {
        let fired = fired.clone();
        let done = done.clone();
        let poll = Poll::new(&mut uv_loop, &read, move |_, _, events| {
            fired.set(Some(events));
            done.set(true);
        })
        .expect("new_poll");
        let mut handle = poll;
        handle
            .poll_start(&mut uv_loop, "r")
            .expect("poll_start(events=\"r\")");
        Rc::new(std::cell::RefCell::new(Some(handle)))
    };

    // Writing to the pipe makes the watched read end readable.
    rustix::io::write(&write, b"readiness").expect("write to pipe");

    let deadline = Instant::now() + TIMEOUT;
    while !done.get() && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump poll loop");
    }
    assert!(done.get(), "poll callback never fired");

    let events = fired.get().expect("poll events");
    assert!(events.readable(), "expected READABLE event, got {events:?}");

    poll.borrow()
        .as_ref()
        .expect("poll handle")
        .close(&mut uv_loop)
        .expect("close poll handle");
}

/// Exercises the poll event-string parser and mask round-trip.
#[test]
fn poll_event_strings_parse_and_report_names() {
    use crate::poll::{PollEvents, poll_start_mask};

    let parsed = poll_start_mask("rwdp");
    assert!(parsed.is_some());

    let events = PollEvents::from_mask(parsed.expect("parsed mask"));
    assert!(events.readable() && events.writable() && events.disconnect() && events.prioritized());
    assert_eq!(events.name(), Some("rwdp"));

    assert!(poll_start_mask("xy").is_none());
    assert_eq!(PollEvents::from_mask(0).name(), None);
}

/// Starting a poll with a `"p"` (PRIORITIZED) mask is rejected with a typed
/// [`NetError::Unsupported`]: the reactor pipeline this handle is built on
/// cannot surface real POLLPRI, so silently mapping it would report false
/// readiness (Task 7c finding 2).
#[test]
fn poll_start_rejects_prioritized_as_unsupported() {
    use crate::Poll;
    use crate::net::NetError;

    let (read, _write) = rustix::pipe::pipe().expect("create pipe");
    let mut uv_loop = UvLoop::new().expect("create loop");
    let mut poll = Poll::new(&mut uv_loop, &read, |_, _, _| {}).expect("new_poll");

    let error = poll
        .poll_start(&mut uv_loop, "p")
        .err()
        .expect("poll_start(events=\"p\") is rejected");
    assert!(
        matches!(error, NetError::Unsupported(_)),
        "expected typed Unsupported, got {error:?}"
    );

    poll.close(&mut uv_loop).expect("close poll handle");
}

// ---------------------------------------------------------------------------
// IPC write2 + pipe_pending_* (luvref 1632-1690, 2091-2130).
// ---------------------------------------------------------------------------

/// Sends a pipe write-end descriptor over a Unix socket pair via `write2`,
/// receiving it on the peer and proving the received descriptor is a working
/// duplicate of the pipe write end. Same-process assert documents the
/// `SCM_RIGHTS` mechanism (luvref 1632-1690).
#[test]
fn ipc_write2_passes_fd_which_receiver_can_use() {
    let (pair_a, pair_b) = mio::net::UnixStream::pair().expect("socketpair");
    let (pipe_read, pipe_write) = rustix::pipe::pipe().expect("create pipe to send");

    let mut uv_loop = UvLoop::new().expect("create loop");
    let receiver = {
        let mut child = Pipe::from_stream(&mut uv_loop, pair_b, true, |_, _, event| match event {
            NetEvent::Read(_) | NetEvent::Eof => {}
            NetEvent::Error(error) => panic!("ipc receive error: {error}"),
            _ => {}
        })
        .expect("wrap receiver pipe");
        child.read_start(&mut uv_loop).expect("read_start ipc pipe");
        std::cell::RefCell::new(Some(child))
    };

    // write2 carries the pipe's write-end file descriptor alongside the data.
    let sender = Pipe::from_stream(&mut uv_loop, pair_a, true, |_, _, event| match event {
        NetEvent::Error(error) => panic!("ipc send error: {error}"),
        _ => {}
    })
    .expect("wrap sender pipe");
    sender
        .write2(&mut uv_loop, b"hi".to_vec(), &pipe_write, PipeHandleKind::Pipe)
        .expect("write2 sends handle");

    // Pump until the receiver's recvmsg has captured the ancillary fd.
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        let ready = {
            let guard = receiver.borrow();
            guard.as_ref().is_some_and(|pipe| pipe.pending_count() > 0)
        };
        if ready {
            break;
        }
        uv_loop.run_nowait().expect("pump ipc loop");
    }
    let receiver = receiver.borrow();
    let child = receiver.as_ref().expect("receiver pipe");
    assert_eq!(child.pending_count(), 1, "uv.pipe_pending_count()");
    assert_eq!(child.pending_type(), Some("pipe"), "uv.pipe_pending_type()");
    child.pending_instances(1); // uv.pipe_pending_instances() stores (no-op on Unix)

    let received_fd = child.pending_take_fd().expect("take pending descriptor");
    rustix::io::write(&received_fd, b"through-passed-fd").expect("write through passed fd");
    let mut buf = [0u8; 64];
    let n = rustix::io::read(&pipe_read, &mut buf).expect("read original pipe");
    assert_eq!(&buf[..n], b"through-passed-fd", "received fd is a working dup of the pipe write end");
}

/// `write2` on a pipe that was not created with the `ipc` option is rejected
/// with a typed error (Task 7c finding 3).
#[test]
fn write2_requires_ipc_pipe() {
    use crate::net::NetError;

    let (pair_a, _pair_b) = mio::net::UnixStream::pair().expect("socketpair");
    let (_, pipe_write) = rustix::pipe::pipe().expect("create pipe");

    let mut uv_loop = UvLoop::new().expect("create loop");
    let sender = Pipe::from_stream(&mut uv_loop, pair_a, false, |_, _, event| match event {
        NetEvent::Error(error) => panic!("send error: {error}"),
        _ => {}
    })
    .expect("wrap non-ipc sender pipe");

    let error = sender
        .write2(&mut uv_loop, b"hi".to_vec(), &pipe_write, PipeHandleKind::Pipe)
        .err()
        .expect("write2 on a non-IPC pipe is rejected");
    assert!(
        matches!(error, NetError::InvalidState(_)),
        "expected typed InvalidState, got {error:?}"
    );
}

/// The pending queue is FIFO: `pending_type` and `pending_take_fd` refer to
/// the same (front) item, in arrival order, even when several descriptors are
/// pending. The first taken fd must be a working dup of the FIRST sent write
/// end, not the last (Task 7c finding 4).
#[test]
fn pending_queue_is_fifo() {
    let (pair_a, pair_b) = mio::net::UnixStream::pair().expect("socketpair");
    let (read_1, write_1) = rustix::pipe::pipe().expect("pipe 1");
    let (read_2, write_2) = rustix::pipe::pipe().expect("pipe 2");

    let mut uv_loop = UvLoop::new().expect("create loop");
    let receiver = {
        let mut child = Pipe::from_stream(&mut uv_loop, pair_b, true, |_, _, event| match event {
            NetEvent::Error(error) => panic!("ipc receive error: {error}"),
            _ => {}
        })
        .expect("wrap receiver pipe");
        child.read_start(&mut uv_loop).expect("read_start ipc pipe");
        std::cell::RefCell::new(Some(child))
    };

    let sender = Pipe::from_stream(&mut uv_loop, pair_a, true, |_, _, event| match event {
        NetEvent::Error(error) => panic!("ipc send error: {error}"),
        _ => {}
    })
    .expect("wrap sender pipe");
    sender
        .write2(&mut uv_loop, b"a".to_vec(), &write_1, PipeHandleKind::Pipe)
        .expect("write2 #1");
    sender
        .write2(&mut uv_loop, b"b".to_vec(), &write_2, PipeHandleKind::Pipe)
        .expect("write2 #2");

    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        let have = receiver
            .borrow()
            .as_ref()
            .is_some_and(|pipe| pipe.pending_count() >= 2);
        if have {
            break;
        }
        uv_loop.run_nowait().expect("pump ipc loop");
    }

    let receiver = receiver.borrow();
    let child = receiver.as_ref().expect("receiver pipe");
    assert_eq!(child.pending_count(), 2, "both descriptors pending");

    // FIFO: the front item reported by pending_type is the one take_fd pops.
    assert_eq!(child.pending_type(), Some("pipe"), "front item pending type");
    let first = child.pending_take_fd().expect("take front pending descriptor");

    // Prove the taken fd is a dup of the FIRST write end (pipe 1): a write
    // through it must land in read_1, while read_2 stays empty.
    rustix::io::write(&first, b"F").expect("write through front fd");

    let probe = |file: &std::fs::File| {
        use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};
        let _ = fcntl_setfl(file, fcntl_getfl(file).expect("getfl") | OFlags::NONBLOCK);
        let mut byte = [0u8; 1];
        match rustix::io::read(file, &mut byte) {
            Ok(1) => Some(byte[0]),
            Ok(_) | Err(_) => None,
        }
    };
    let read_1 = std::fs::File::from(read_1);
    let read_2 = std::fs::File::from(read_2);
    assert_eq!(probe(&read_1), Some(b'F'), "front-taken fd reaches pipe 1");
    assert_eq!(probe(&read_2), None, "second pipe must remain untouched by the front take");
}

// ---------------------------------------------------------------------------
// Extra stdio (fd >= 3) (luvref 1434-1447).
// ---------------------------------------------------------------------------

/// Spawning with a `CreatePipe` extra descriptor at an exact fd ≥ 3 is
/// rejected with a typed [`ProcessError::Unsupported`] instead of silently
/// installing the pipe at whatever descriptor number the parent happens to
/// have free. False success is worse than an honest rejection (Task 7c
/// finding 1); `Inherit`/`Ignore` extra entries still need no parent action.
#[test]
fn extra_stdio_create_pipe_is_typed_unsupported() {
    let mut uv_loop = UvLoop::new().expect("create loop");
    let mut options = SpawnOptions::new("/bin/true");
    options.extra_stdio = vec![ExtraStdio {
        fd: 3,
        config: StdioConfig::CreatePipe,
    }];

    let error = process::spawn(&mut uv_loop, options, |_, _| {})
        .err()
        .expect("exact-fd extra stdio CreatePipe is rejected");
    assert!(
        matches!(&error, crate::process::ProcessError::Unsupported { .. }),
        "expected typed Unsupported, got {error:?}"
    );
}

/// `Inherit` extra entries on fd ≥ 3 are accepted (they need no parent-side
/// descriptor manipulation) and spawn proceeds to launch the child.
#[test]
fn extra_stdio_inherit_fd3_spawns_ok() {
    let mut uv_loop = UvLoop::new().expect("create loop");
    let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut options = SpawnOptions::new("/bin/true");
    options.extra_stdio = vec![ExtraStdio {
        fd: 3,
        config: StdioConfig::Inherit,
    }];

    let spawned = {
        let exited = exited.clone();
        process::spawn(&mut uv_loop, options, move |_, _exit| {
            exited.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("Inherit extra stdio spawns")
    };
    assert_eq!(spawned.extra.len(), 0, "no created endpoints for Inherit entries");

    let deadline = Instant::now() + TIMEOUT;
    while !exited.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump spawn loop");
    }
    assert!(exited.load(std::sync::atomic::Ordering::SeqCst), "child never exited");
}

// ---------------------------------------------------------------------------
// Vectored fs (luvref 2966-3010 buffer forms; writev=preadv/pwritev).
// ---------------------------------------------------------------------------

#[test]
fn vectored_fs_writev_and_readv_split_correctly() {
    use crate::fs;
    use std::io::Read as _;

    let path = temp_path("vectored");
    let handle = fs::open(&path, fs::OpenFlags::WRITE, 0o644)
        .expect("open for write");
    let written = fs::writev(&handle, &[b"Hello".to_vec(), b" ".to_vec(), b"World".to_vec()], None)
        .expect("uv.fs_write table-of-buffers");
    assert_eq!(written, 11, "writev total");
    fs::close(&handle).expect("close");

    let mut content = String::new();
    std::fs::File::open(&path)
        .expect("open")
        .read_to_string(&mut content)
        .expect("read");
    assert_eq!(content, "Hello World", "writev concatenation persisted");

    // readv into two buffers splits the payload correctly.
    let handle = fs::open(&path, fs::OpenFlags::READ, 0).expect("open for read");
    let parts = fs::readv(&handle, &[5, 6], Some(0)).expect("uv.fs_read table-of-buffers");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], b"Hello".to_vec());
    assert_eq!(parts[1], b" World".to_vec());
    assert_eq!(parts.concat(), b"Hello World".to_vec());
    fs::close(&handle).expect("close");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Misc surface (luvref 4032-4463): version, exepath, passwd, rusage, memory,
// loadavg, uptime, cpu_info, print handles.
// ---------------------------------------------------------------------------

#[test]
fn misc_surface_returns_sane_values() {
    use crate::misc;

    assert_eq!(misc::version() & 0xFF, 0, "axis version 0.1.0 patch");
    assert_eq!(misc::version_string(), env!("CARGO_PKG_VERSION"));

    let exe = misc::exepath().expect("uv.exepath");
    assert!(!exe.as_os_str().is_empty());

    let passwd = misc::os_get_passwd().expect("uv.os_get_passwd");
    assert!(!passwd.username.is_empty(), "username present");
    assert!(!passwd.homedir.is_empty(), "homedir present");

    let unsupported = misc::os_setenv("OX_TEST", "1");
    assert!(unsupported.is_err(), "os_setenv is typed unsupported (no safe setter)");
    let unsupported = misc::os_unsetenv("OX_TEST");
    assert!(unsupported.is_err(), "os_unsetenv is typed unsupported");

    let rusage = misc::getrusage().expect("uv.getrusage");
    assert!(rusage.nvcsw > 0 || rusage.stime.0 > 0 || rusage.utime.0 > 0, "some rusage populated");

    let rss = misc::resident_set_memory().expect("uv.resident_set_memory");
    assert!(rss > 0, "RSS positive");

    let total = misc::get_total_memory();
    let free = misc::get_free_memory();
    assert!(total > 0, "MemTotal positive");
    assert!(free > 0, "MemAvailable positive");
    let _ = misc::get_constrained_memory();
    let _ = misc::get_available_memory();

    let (one, five, fifteen) = misc::loadavg();
    assert!(one >= 0.0 && five >= 0.0 && fifteen >= 0.0, "loadavg triad non-negative");

    let uptime = misc::uptime().expect("uv.uptime");
    assert!(uptime > 0.0, "uptime positive");

    let cpus = misc::cpu_info().expect("uv.cpu_info");
    assert!(!cpus.is_empty(), "at least one CPU");

    // print_*_handles is an ad hoc stderr debug aid; it must not panic.
    let mut loop_for_print = UvLoop::new().expect("create loop");
    misc::print_all_handles(&mut loop_for_print);
    misc::print_active_handles(&mut loop_for_print);
}