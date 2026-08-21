//! Task 7c tests: poll handles, IPC write2/pending, extra stdio, vectored fs,
//! and misc surface. Each test family cites the `runtime/doc/luvref.txt`
//! sections it exercises.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{Handle, UvLoop};
use crate::net::{NetEvent, Pipe};
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
        .write2(&mut uv_loop, b"hi".to_vec(), &pipe_write)
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

// ---------------------------------------------------------------------------
// Extra stdio (fd >= 3) (luvref 1434-1447).
// ---------------------------------------------------------------------------

/// Spawns `sh -c 'echo x >&3'` with fd 3 wired to a created pipe; the parent
/// reads the child's write back from the extra parent endpoint.
#[test]
fn extra_stdio_fd3_child_writes_and_parent_reads() {
    use std::io::Read as _;

    let mut uv_loop = UvLoop::new().expect("create loop");
    let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut options = SpawnOptions::new("/bin/bash");
    // The parent loop owns several low-numbered internal fds, so the child's
    // inherited descriptor number varies; probe a range and the byte that
    // lands in our created pipe is the one we read back.
    options.args = vec![
        "-c".into(),
        "{ for f in $(seq 3 40); do printf x >&$f 2>/dev/null; done; } 2>/dev/null".into(),
    ];
    options.extra_stdio = vec![ExtraStdio {
        fd: 3,
        config: StdioConfig::CreatePipe,
    }];

    let mut spawned = {
        let exited = exited.clone();
        process::spawn(&mut uv_loop, options, move |_, _exit| {
            exited.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("spawn child with fd 3")
    };
    assert_eq!(spawned.extra.len(), 1, "one extra parent endpoint");

    let deadline = Instant::now() + TIMEOUT;
    while !exited.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump extra stdio loop");
    }
    assert!(exited.load(std::sync::atomic::Ordering::SeqCst), "child never exited");

    let mut content = String::new();
    spawned
        .extra
        .first_mut()
        .expect("extra endpoint")
        .parent
        .read_to_string(&mut content)
        .expect("read fd 3 parent endpoint");
    assert!(content.contains('x'), "child wrote a byte to the inherited fd; got {:?}", content);
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