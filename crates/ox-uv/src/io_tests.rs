use std::cell::{Cell, RefCell};
use std::fs as std_fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::UvLoop;
use crate::dns::{self, AddrInfoHints};
use crate::fs::{self, OpenFlags};
use crate::net::{NetEvent, Tcp, Udp};
use crate::pool::{LoopPoster, Pool};
use crate::process::{self, SpawnOptions};
#[cfg(unix)]
use crate::process::StdioConfig;
use crate::work;
use crate::{Handle, HandleId};

const TIMEOUT: Duration = Duration::from_secs(10);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ox-uv-io-tests-{}-{sequence}",
            std::process::id()
        ));
        std_fs::create_dir(&path).expect("create isolated test directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std_fs::remove_dir_all(&self.0);
    }
}

fn run_default_bounded(uv_loop: &mut UvLoop) {
    let poster = uv_loop.completion_poster();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let watchdog = thread::spawn(move || {
        if matches!(cancel_rx.recv_timeout(TIMEOUT), Err(mpsc::RecvTimeoutError::Timeout)) {
            let _ = poster.post(Box::new(|uv_loop| uv_loop.stop()));
        }
    });
    let alive = uv_loop.run_default().expect("pump loop");
    let _ = cancel_tx.send(());
    watchdog.join().expect("join loop watchdog");
    assert!(!alive, "loop still had live work after its deadline");
}

#[test]
fn sync_filesystem_round_trip_and_enoent_name() {
    let temp = TempDir::new();
    let original = temp.path().join("b-original");
    let copy = temp.path().join("a-copy");
    let renamed = temp.path().join("c-renamed");
    let payload = b"filesystem round trip";

    std_fs::write(&original, []).expect("seed file");
    let file = fs::open(&original, OpenFlags::READ_WRITE, 0o600).expect("open file");
    assert_eq!(fs::write(&file, payload, Some(0)).expect("write file"), payload.len());
    assert_eq!(fs::read(&file, payload.len(), Some(0)).expect("read file"), payload);
    assert_eq!(fs::fstat(&file).expect("stat open file").size, payload.len() as u64);
    fs::close(&file).expect("close file");

    assert_eq!(fs::copyfile(&original, &copy, false).expect("copy file"), payload.len() as u64);
    let names: Vec<_> = fs::scandir(temp.path())
        .expect("scan directory")
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["a-copy", "b-original"]);

    fs::rename(&copy, &renamed).expect("rename file");
    assert_eq!(fs::stat(&renamed).expect("stat renamed file").size, payload.len() as u64);
    fs::unlink(&original).expect("unlink original");
    fs::unlink(&renamed).expect("unlink renamed file");

    let error = fs::stat(temp.path().join("absent")).expect_err("missing path must fail");
    assert_eq!(error.name, "ENOENT");
}

#[test]
fn async_filesystem_callback_waits_for_loop_and_runs_on_loop_thread() {
    let temp = TempDir::new();
    let path = temp.path().join("async-file");
    std_fs::write(&path, b"async").expect("seed async file");

    let pool = Pool::with_size(1);
    let mut uv_loop = UvLoop::new().expect("create loop");
    let poster = uv_loop.completion_poster();
    let loop_thread = thread::current().id();
    let (work_tx, work_rx) = mpsc::channel();
    let (callback_tx, callback_rx) = mpsc::channel();

    fs::run_async(
        &pool,
        poster,
        move || {
            work_tx.send(()).expect("signal worker completion");
            fs::stat(path)
        },
        move |_, result| {
            callback_tx
                .send((thread::current().id(), result.expect("async stat").size))
                .expect("send callback result");
        },
    )
    .expect("submit async stat");

    work_rx.recv_timeout(TIMEOUT).expect("worker finishes");
    assert!(callback_rx.try_recv().is_err(), "callback ran before loop pumping");
    run_default_bounded(&mut uv_loop);
    assert_eq!(callback_rx.recv_timeout(TIMEOUT).expect("callback result"), (loop_thread, 5));
}

#[test]
fn fixed_pool_drains_thirty_two_jobs_without_deadlock() {
    let pool = Pool::with_size(4);
    assert_eq!(pool.size(), 4);
    let mut uv_loop = UvLoop::new().expect("create loop");
    let poster = uv_loop.completion_poster();
    let (tx, rx) = mpsc::channel();

    for value in 0_u32..32 {
        let tx = tx.clone();
        pool.submit(poster.clone(), move || value * value, move |_, result| {
            tx.send(result.expect("pool work succeeds")).expect("send pool result");
        })
        .expect("submit pool job");
    }
    drop(tx);

    run_default_bounded(&mut uv_loop);
    let mut results: Vec<_> = (0..32)
        .map(|_| rx.recv_timeout(TIMEOUT).expect("receive every pool result"))
        .collect();
    results.sort_unstable();
    let mut expected: Vec<_> = (0_u32..32).map(|value| value * value).collect();
    expected.sort_unstable();
    assert_eq!(results, expected);
}

#[test]
fn queue_work_returns_value_on_loop_thread() {
    let pool = Pool::with_size(2);
    let mut uv_loop = UvLoop::new().expect("create loop");
    let loop_thread = thread::current().id();
    let (tx, rx) = mpsc::channel();
    let queued = work::new_work(
        pool,
        uv_loop.completion_poster(),
        |data| {
            let value = *data.downcast::<u32>().expect("u32 work input");
            Box::new(value + 1)
        },
        move |_, result| {
            let value = *result.expect("work succeeds").downcast::<u32>().expect("u32 work output");
            tx.send((thread::current().id(), value)).expect("send work result");
        },
    );

    work::queue_work(&queued, Box::new(41_u32)).expect("queue work");
    run_default_bounded(&mut uv_loop);
    assert_eq!(rx.recv_timeout(TIMEOUT).expect("receive work result"), (loop_thread, 42));
}

#[test]
fn new_thread_join_returns_entry_value() {
    let mut child = crate::thread::new_thread(None, |_| 42_u32).expect("start thread");
    assert_eq!(child.join().expect("join thread"), 42);
}

#[cfg(unix)]
#[test]
fn process_cat_echoes_through_pipes_and_exits_cleanly() {
    use crate::net::NetEvent;

    let mut uv_loop = UvLoop::new().expect("create loop");
    let (tx, rx) = mpsc::channel();
    let mut options = SpawnOptions::new("/bin/cat");
    options.stdio = [StdioConfig::CreatePipe, StdioConfig::CreatePipe, StdioConfig::Ignore];
    let mut spawned = process::spawn(&mut uv_loop, options, move |_, result| {
        tx.send(result.expect("cat exits cleanly")).expect("send cat exit");
    })
    .expect("spawn cat");

    let payload = b"cat pipe echo\n";
    let echoed: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
    let eof_reached: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    {
        let mut stdin = spawned.pipes.stdin.take().expect("cat stdin pipe");
        stdin.write(&mut uv_loop, payload.to_vec()).expect("write cat stdin");
        // The small write flushes synchronously; closing stdin signals EOF.
        stdin.close(&mut uv_loop).expect("close cat stdin");
    }
    {
        let echoed = echoed.clone();
        let eof_reached = eof_reached.clone();
        let mut stdout = spawned.pipes.stdout.take().expect("cat stdout pipe");
        stdout
            .read_start(&mut uv_loop, move |_, _, event| match event {
                NetEvent::Read(data) => echoed.borrow_mut().extend(data),
                NetEvent::Eof => eof_reached.set(true),
                NetEvent::Error(error) => panic!("cat stdout read error: {error}"),
                _ => {}
            })
            .expect("start reading cat stdout");
    }

    run_default_bounded(&mut uv_loop);
    assert!(eof_reached.get(), "cat stdout did not reach EOF");
    assert_eq!(&*echoed.borrow(), &payload);

    let exit = rx.recv_timeout(TIMEOUT).expect("cat exit callback");
    assert_eq!((exit.code, exit.signal), (0, 0));
}

#[test]
fn missing_executable_returns_spawn_error() {
    let mut uv_loop = UvLoop::new().expect("create loop");
    let missing = SpawnOptions::new("/definitely/not/an/ox-uv-executable");
    assert!(process::spawn(&mut uv_loop, missing, |_, _| {}).is_err());
}

#[cfg(unix)]
#[test]
fn sigterm_is_reported_as_terminating_signal() {
    let mut uv_loop = UvLoop::new().expect("create loop");
    let (tx, rx) = mpsc::channel();
    let mut options = SpawnOptions::new("/bin/sh");
    options.args = vec!["-c".into(), "exec sleep 30".into()];
    options.stdio = [StdioConfig::Ignore; 3];
    let spawned = process::spawn(&mut uv_loop, options, move |_, result| {
        tx.send(result.expect("sleeper exits by signal")).expect("send terminated exit");
    })
    .expect("spawn sleeper");

    process::process_kill(&spawned.process, Some(15)).expect("send SIGTERM");
    run_default_bounded(&mut uv_loop);
    let exit = rx.recv_timeout(TIMEOUT).expect("terminated exit callback");
    assert_eq!((exit.code, exit.signal), (0, 15));
}

#[cfg(unix)]
#[test]
fn pty_reports_terminal_and_supports_resize() {
    use crate::net::NetEvent;
    use crate::process::PtySize;

    let mut uv_loop = UvLoop::new().expect("create loop");
    let (tx, rx) = mpsc::channel();
    let mut options = SpawnOptions::new("/bin/sh");
    options.args = vec!["-c".into(), "tty".into()];
    let mut spawned = process::spawn_pty(
        &mut uv_loop,
        options,
        PtySize { rows: 24, columns: 80 },
        move |_, result| {
            tx.send(result.expect("PTY exits cleanly")).expect("send PTY exit");
        },
    )
    .expect("spawn PTY");

    spawned.master.resize(PtySize { rows: 40, columns: 100 }).expect("resize PTY");
    let size = spawned.master.get_size().expect("read PTY size");
    assert_eq!((size.rows, size.columns), (40, 100));

    let output: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
    let eof_reached: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    {
        let output = output.clone();
        let eof_reached = eof_reached.clone();
        spawned
            .master
            .read_start(&mut uv_loop, move |_, _, event| match event {
                NetEvent::Read(data) => output.borrow_mut().extend(data),
                NetEvent::Eof => eof_reached.set(true),
                NetEvent::Error(error) => panic!("PTY read error: {error}"),
                _ => {}
            })
            .expect("start reading PTY");
    }

    let deadline = Instant::now() + TIMEOUT;
    while !eof_reached.get() && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump PTY");
    }
    assert!(eof_reached.get(), "PTY did not reach EOF");
    let output = String::from_utf8(output.borrow().clone()).expect("tty output is UTF-8");
    #[cfg(target_os = "linux")]
    assert!(output.trim().starts_with("/dev/pts/"), "unexpected tty path: {output:?}");
    #[cfg(not(target_os = "linux"))]
    assert!(output.trim().starts_with("/dev/"), "unexpected tty path: {output:?}");

    spawned.master.close(&mut uv_loop).expect("close PTY master");
    run_default_bounded(&mut uv_loop);
    let exit = rx.recv_timeout(TIMEOUT).expect("PTY exit callback");
    assert_eq!((exit.code, exit.signal), (0, 0));
}

#[cfg(unix)]
#[test]
fn pty_sigterm_reports_terminating_signal() {
    use crate::process::PtySize;

    let mut uv_loop = UvLoop::new().expect("create loop");
    let (tx, rx) = mpsc::channel();
    let mut options = SpawnOptions::new("/bin/sh");
    options.args = vec!["-c".into(), "exec sleep 30".into()];
    let spawned = process::spawn_pty(
        &mut uv_loop,
        options,
        PtySize { rows: 24, columns: 80 },
        move |_, result| {
            tx.send(result.expect("PTY sleeper exits by signal")).expect("send terminated PTY exit");
        },
    )
    .expect("spawn PTY sleeper");

    process::process_kill(&spawned.process, Some(15)).expect("send SIGTERM to PTY child");
    run_default_bounded(&mut uv_loop);
    let exit = rx.recv_timeout(TIMEOUT).expect("PTY terminated exit callback");
    assert_eq!((exit.code, exit.signal), (0, 15));
}

#[test]
fn tcp_loopback_drains_large_echo_across_read_events() {
    let mut uv_loop = UvLoop::new().expect("create loop");

    let server: Rc<RefCell<Option<Box<Tcp>>>> = Rc::new(RefCell::new(None));
    let server_id: Rc<Cell<Option<HandleId>>> = Rc::new(Cell::new(None));
    let server_read_events = Rc::new(Cell::new(0usize));
    let echoed = Rc::new(RefCell::new(Vec::<u8>::new()));
    let done = Rc::new(Cell::new(false));
    let payload: Vec<u8> = (0..(192 * 1024)).map(|index| (index % 251) as u8).collect();
    let payload_len = payload.len();

    let listener = {
        let server = server.clone();
        let server_id = server_id.clone();
        let server_read_events = server_read_events.clone();
        let mut listener = Tcp::bind(
            &mut uv_loop,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            move |uv_loop, id, event| match event {
                NetEvent::AcceptedTcp(mut accepted) => {
                    accepted.read_start(uv_loop).expect("start server reads");
                    server_id.set(Some(accepted.id()));
                    *server.borrow_mut() = Some(accepted);
                }
                NetEvent::Read(data) if server_id.get() == Some(id) => {
                    server_read_events.set(server_read_events.get() + 1);
                    if let Some(stream) = server.borrow_mut().as_mut() {
                        stream.write(uv_loop, data).expect("echo TCP data");
                    }
                }
                NetEvent::WriteComplete { result, .. } if server_id.get() == Some(id) => {
                    result.expect("server write succeeds");
                }
                NetEvent::Error(error) => panic!("TCP server error: {error}"),
                _ => {}
            },
        )
        .expect("bind TCP");
        listener.listen(&mut uv_loop, 16).expect("listen TCP");
        Rc::new(RefCell::new(listener))
    };
    let address = listener.borrow().local_addr().expect("listener address");

    let client = {
        let echoed = echoed.clone();
        let done = done.clone();
        let mut client = Tcp::connect(
            &mut uv_loop,
            address,
            move |_, _, event| match event {
                NetEvent::Connected(result) => result.expect("TCP connection succeeds"),
                NetEvent::Read(data) => {
                    let mut echoed = echoed.borrow_mut();
                    echoed.extend(data);
                    if echoed.len() == payload_len {
                        done.set(true);
                    }
                }
                NetEvent::Eof => done.set(true),
                NetEvent::WriteComplete { result, .. } => result.expect("client write succeeds"),
                NetEvent::Error(error) => panic!("TCP client error: {error}"),
                _ => {}
            },
        )
        .expect("connect TCP");
        client.read_start(&mut uv_loop).expect("start client reads");
        client.write(&mut uv_loop, payload.clone()).expect("queue TCP payload");
        Rc::new(RefCell::new(client))
    };

    let deadline = Instant::now() + TIMEOUT;
    while !done.get() && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump TCP");
    }
    assert!(done.get(), "TCP echo timed out");

    assert_eq!(&*echoed.borrow(), &payload);
    assert!(
        server_read_events.get() > 1,
        "large TCP payload was not delivered as partial reads"
    );

    server
        .borrow()
        .as_ref()
        .expect("server stream")
        .close(&mut uv_loop)
        .expect("close TCP server stream");
    client
        .borrow()
        .close(&mut uv_loop)
        .expect("close TCP client");
    listener
        .borrow()
        .close(&mut uv_loop)
        .expect("close TCP listener");
    for _ in 0..8 {
        let _ = uv_loop.run_nowait();
    }
}

#[cfg(unix)]
#[test]
fn unix_pipe_loopback_echoes_payload() {
    use crate::net::Pipe;

    let mut uv_loop = UvLoop::new().expect("create loop");
    let temp = TempDir::new();
    let socket = temp.path().join("echo.sock");

    let server: Rc<RefCell<Option<Box<Pipe>>>> = Rc::new(RefCell::new(None));
    let server_id: Rc<Cell<Option<HandleId>>> = Rc::new(Cell::new(None));
    let echoed = Rc::new(RefCell::new(Vec::<u8>::new()));
    let done = Rc::new(Cell::new(false));
    let payload = b"unix pipe echo".to_vec();

    let listener = {
        let server = server.clone();
        let server_id = server_id.clone();
        let mut listener = Pipe::bind(
            &mut uv_loop,
            &socket,
            move |uv_loop, id, event| match event {
                NetEvent::AcceptedPipe(mut accepted) => {
                    accepted.read_start(uv_loop).expect("start server pipe reads");
                    server_id.set(Some(accepted.id()));
                    *server.borrow_mut() = Some(accepted);
                }
                NetEvent::Read(data) if server_id.get() == Some(id) => {
                    if let Some(stream) = server.borrow_mut().as_mut() {
                        stream.write(uv_loop, data).expect("echo pipe data");
                    }
                }
                NetEvent::WriteComplete { result, .. } if server_id.get() == Some(id) => {
                    result.expect("server write succeeds");
                }
                NetEvent::Error(error) => panic!("pipe server error: {error}"),
                _ => {}
            },
        )
        .expect("bind pipe");
        listener.listen(&mut uv_loop, 8).expect("listen pipe");
        Rc::new(RefCell::new(listener))
    };

    let client = {
        let echoed = echoed.clone();
        let done = done.clone();
        let payload_len = payload.len();
        let mut client = Pipe::connect(
            &mut uv_loop,
            &socket,
            move |_, _, event| match event {
                NetEvent::Connected(result) => result.expect("pipe connection succeeds"),
                NetEvent::Read(data) => {
                    let mut echoed = echoed.borrow_mut();
                    echoed.extend(data);
                    if echoed.len() == payload_len {
                        done.set(true);
                    }
                }
                NetEvent::Eof => done.set(true),
                NetEvent::WriteComplete { result, .. } => result.expect("client write succeeds"),
                NetEvent::Error(error) => panic!("pipe client error: {error}"),
                _ => {}
            },
        )
        .expect("connect pipe");
        client.read_start(&mut uv_loop).expect("start pipe client reads");
        client.write(&mut uv_loop, payload.clone()).expect("queue pipe payload");
        Rc::new(RefCell::new(client))
    };

    let deadline = Instant::now() + TIMEOUT;
    while !done.get() && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump pipe");
    }
    assert!(done.get(), "pipe echo timed out");

    assert_eq!(&*echoed.borrow(), &payload);

    server
        .borrow()
        .as_ref()
        .expect("server pipe")
        .close(&mut uv_loop)
        .expect("close server pipe");
    client
        .borrow()
        .close(&mut uv_loop)
        .expect("close pipe client");
    listener
        .borrow()
        .close(&mut uv_loop)
        .expect("close pipe listener");
    for _ in 0..8 {
        let _ = uv_loop.run_nowait();
    }
}

#[test]
fn udp_loopback_echoes_datagram() {
    let mut uv_loop = UvLoop::new().expect("create loop");

    let echoed = Rc::new(RefCell::new(None::<Vec<u8>>));
    let done = Rc::new(Cell::new(false));
    let payload = b"udp echo".to_vec();

    let server_holder: Rc<RefCell<Option<Udp>>> = Rc::new(RefCell::new(None));
    let server_for_cb = server_holder.clone();

    let server_address = {
        let mut server = Udp::bind(
            &mut uv_loop,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            move |uv_loop, _id, event| match event {
                NetEvent::Datagram { data, from } => {
                    if let Some(server) = server_for_cb.borrow_mut().as_mut() {
                        server.send(uv_loop, data, Some(from)).expect("queue UDP echo");
                    }
                }
                NetEvent::WriteComplete { result, .. } => result.expect("server send succeeds"),
                NetEvent::Error(error) => panic!("UDP server error: {error}"),
                _ => {}
            },
        )
        .expect("bind UDP server");
        let address = server.local_addr().expect("UDP server address");
        server.recv_start(&mut uv_loop).expect("start UDP server receive");
        *server_holder.borrow_mut() = Some(server);
        address
    };

    let client = {
        let echoed = echoed.clone();
        let done = done.clone();
        let mut client = Udp::bind(
            &mut uv_loop,
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            move |_, _, event| match event {
                NetEvent::Datagram { data, from } => {
                    assert_eq!(from, server_address);
                    *echoed.borrow_mut() = Some(data);
                    done.set(true);
                }
                NetEvent::WriteComplete { result, .. } => result.expect("client send succeeds"),
                NetEvent::Error(error) => panic!("UDP client error: {error}"),
                _ => {}
            },
        )
        .expect("bind UDP client");
        let _client_address = client.local_addr().expect("UDP client address");
        client.recv_start(&mut uv_loop).expect("start UDP client receive");
        client
            .send(&mut uv_loop, payload.clone(), Some(server_address))
            .expect("queue UDP datagram");
        Rc::new(RefCell::new(client))
    };

    let deadline = Instant::now() + TIMEOUT;
    while !done.get() && Instant::now() < deadline {
        uv_loop.run_nowait().expect("pump UDP");
    }
    assert!(done.get(), "UDP echo timed out");

    assert_eq!(
        echoed.borrow().as_ref().expect("echoed UDP datagram").as_slice(),
        payload.as_slice()
    );

    server_holder
        .borrow()
        .as_ref()
        .expect("UDP server")
        .close(&mut uv_loop)
        .expect("close UDP server");
    client
        .borrow()
        .close(&mut uv_loop)
        .expect("close UDP client");
    for _ in 0..8 {
        let _ = uv_loop.run_nowait();
    }
}

#[test]
fn dns_localhost_resolves_to_loopback() {
    let addresses = dns::getaddrinfo(Some("localhost"), Some("0"), AddrInfoHints::default())
        .expect("resolve localhost");
    assert!(
        addresses.iter().any(|entry| match entry.address {
            IpAddr::V4(address) => address.is_loopback(),
            IpAddr::V6(address) => address.is_loopback(),
        }),
        "localhost did not resolve to a loopback address: {addresses:?}"
    );
}
