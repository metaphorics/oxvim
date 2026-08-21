use std::io::{Read, Write};
use std::net::{TcpStream as StdTcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mio::net::TcpListener;
use mio::{Events as MioEvents, Interest, Token};

use crate::{
    DrainState, Event, IO_TOKEN_START, Loop, MultiQueue, Reactor, TimerEntry, TimerHeap,
    WaitOutcome,
};

#[test]
fn timer_orders_equal_deadlines_cancels_and_rearms() {
    let deadline = Instant::now() + Duration::from_millis(10);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut timers = TimerHeap::new();
    let first_observed = Arc::clone(&observed);
    timers.insert(
        deadline,
        TimerEntry::once(move || first_observed.lock().unwrap().push(1)),
    );
    let cancelled = timers.insert(deadline, TimerEntry::once(|| panic!("cancelled timer fired")));
    assert!(timers.cancel(cancelled).is_some());
    let second_observed = Arc::clone(&observed);
    timers.insert(
        deadline,
        TimerEntry::once(move || second_observed.lock().unwrap().push(2)),
    );

    for mut timer in timers.expired(deadline) {
        timer.fire();
    }
    assert_eq!(*observed.lock().unwrap(), vec![1, 2]);

    let repeats = Arc::new(Mutex::new(0));
    let repeat_count = Arc::clone(&repeats);
    let repeat_id = timers.insert(
        deadline,
        TimerEntry::repeating(Duration::from_millis(5), move || {
            *repeat_count.lock().unwrap() += 1;
        }),
    );
    for mut timer in timers.expired(deadline) {
        timer.fire();
    }
    assert_eq!(*repeats.lock().unwrap(), 1);
    assert_eq!(timers.next_deadline(), Some(deadline + Duration::from_millis(5)));
    assert!(timers.cancel(repeat_id).is_some());
}

#[test]
fn multiqueue_selective_drain_keeps_sibling_events() {
    let mut queues = MultiQueue::new();
    let root = queues.root();
    let outer = queues.child(root).unwrap();
    let nested = queues.child(outer).unwrap();
    let sibling = queues.child(root).unwrap();
    queues.put(outer, Event::Signal(10)).unwrap();
    queues.put(sibling, Event::Signal(20)).unwrap();
    queues.put(nested, Event::Signal(30)).unwrap();

    let drained = queues.process_events(outer).unwrap();
    let signals: Vec<_> = drained
        .into_iter()
        .map(|event| match event {
            Event::Signal(signal) => signal,
            Event::Callback(_) => unreachable!(),
        })
        .collect();
    assert_eq!(signals, vec![10, 30]);
    assert_eq!(queues.len(sibling).unwrap(), 1);
    assert_eq!(queues.len(root).unwrap(), 1);

    let sibling_event = queues.process_events(root).unwrap().pop().unwrap();
    assert!(matches!(sibling_event, Event::Signal(20)));
}

#[test]
fn deferred_work_from_thread_runs_on_loop_thread() {
    let mut event_loop = Loop::new().unwrap();
    let owner = event_loop.root();
    let scheduler = event_loop.scheduler();
    let loop_thread = thread::current().id();
    let observed = Arc::new(Mutex::new(None));
    let callback_observed = Arc::clone(&observed);
    thread::spawn(move || {
        scheduler
            .schedule_deferred(
                owner,
                Event::callback(move || {
                    *callback_observed.lock().unwrap() = Some(thread::current().id());
                }),
            )
            .unwrap();
    })
    .join()
    .unwrap();

    event_loop.run_once(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(*observed.lock().unwrap(), Some(loop_thread));
}

#[test]
fn recursive_wait_drains_only_selected_owner() {
    let mut event_loop = Loop::new().unwrap();
    let root = event_loop.root();
    let channel = event_loop.events().child(root).unwrap();
    let sibling = event_loop.events().child(root).unwrap();
    let returned = Arc::new(AtomicBool::new(false));
    let callback_returned = Arc::clone(&returned);
    event_loop
        .events()
        .put(
            channel,
            Event::callback(move || callback_returned.store(true, Ordering::Release)),
        )
        .unwrap();
    event_loop
        .events()
        .put(sibling, Event::Signal(99))
        .unwrap();

    let condition = Arc::clone(&returned);
    let outcome = event_loop
        .process_events_until(
            channel,
            move || condition.load(Ordering::Acquire),
            Some(Duration::from_secs(1)),
        )
        .unwrap();
    assert_eq!(outcome, WaitOutcome::ConditionMet);
    assert_eq!(event_loop.events().len(sibling).unwrap(), 1);
    assert_eq!(event_loop.events().len(root).unwrap(), 1);
}

#[test]
fn timer_phase_precedes_deferred_safe_point() {
    let mut event_loop = Loop::new().unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let timer_order = Arc::clone(&order);
    event_loop.timers().insert(
        Instant::now(),
        TimerEntry::once(move || timer_order.lock().unwrap().push("timer")),
    );
    let deferred_order = Arc::clone(&order);
    event_loop
        .scheduler()
        .schedule_deferred(
            event_loop.root(),
            Event::callback(move || deferred_order.lock().unwrap().push("deferred")),
        )
        .unwrap();

    event_loop.run_once(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["timer", "deferred"]);
}

#[test]
fn fast_work_runs_before_deferred_safe_point() {
    let mut event_loop = Loop::new().unwrap();
    let owner = event_loop.root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let deferred_order = Arc::clone(&order);
    event_loop
        .scheduler()
        .schedule_deferred(
            owner,
            Event::callback(move || deferred_order.lock().unwrap().push("deferred")),
        )
        .unwrap();
    let fast_order = Arc::clone(&order);
    event_loop
        .work_queues()
        .schedule_fast(
            owner,
            Event::callback(move || fast_order.lock().unwrap().push("fast")),
        )
        .unwrap();

    event_loop.run_once(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["fast", "deferred"]);
}

#[cfg(unix)]
#[test]
fn signal_is_delivered_as_loop_event() {
    use signal_hook::consts::SIGUSR1;

    let mut event_loop = Loop::with_signals(&[SIGUSR1]).unwrap();
    signal_hook::low_level::raise(SIGUSR1).unwrap();
    let events = event_loop.run_once(Some(Duration::from_secs(1))).unwrap();
    assert!(events
        .into_iter()
        .any(|event| matches!(event, Event::Signal(SIGUSR1))));
}

#[cfg(unix)]
#[test]
fn internal_sources_never_collide_with_public_tokens() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    use mio::unix::SourceFd;

    use crate::SIGNAL_TOKEN;

    let mut event_loop = Loop::new().unwrap();

    let alloc_internal = |event_loop: &mut Loop| {
        // A raw fd decoupled from the loop; registration is what is under test.
        let (read, _write) = UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let fd = read.as_raw_fd();
        let mut source = SourceFd(&fd);
        let token = event_loop
            .register_internal(&mut source, Interest::READABLE)
            .unwrap();
        // The allocated token lives strictly between the reserved signal token
        // and the public range; it must never step on either.
        assert!(token.0 > SIGNAL_TOKEN.0 && token.0 < IO_TOKEN_START);
        token
    };

    let first = alloc_internal(&mut event_loop);
    let second = alloc_internal(&mut event_loop);
    assert_ne!(first, second, "repeat internal allocations must be distinct");

    // A caller-owned source at the first public token registers with no
    // DuplicateReadiness collision against the internal tokens.
    let (read, _write) = UnixStream::pair().unwrap();
    read.set_nonblocking(true).unwrap();
    let fd = read.as_raw_fd();
    let mut public_source = SourceFd(&fd);
    let public = Token(IO_TOKEN_START);
    event_loop
        .reactor()
        .register(&mut public_source, public, Interest::READABLE)
        .unwrap();
    event_loop
        .on_readiness(public, |_, _| Ok(DrainState::Drained))
        .expect("public token must not collide with internal registrations");
}

#[test]
fn mio_reactor_echoes_loopback_tcp() {
    let mut reactor = Reactor::new().unwrap();
    let address = "127.0.0.1:0".to_socket_addrs().unwrap().next().unwrap();
    let mut listener = TcpListener::bind(address).unwrap();
    let address = listener.local_addr().unwrap();
    let listener_token = Token(IO_TOKEN_START);
    reactor
        .register(&mut listener, listener_token, Interest::READABLE)
        .unwrap();

    let client = thread::spawn(move || {
        let mut stream = StdTcpStream::connect(address).unwrap();
        stream.write_all(b"ping").unwrap();
        let mut response = [0; 4];
        stream.read_exact(&mut response).unwrap();
        response
    });

    let mut events = MioEvents::with_capacity(8);
    reactor
        .poll(&mut events, Some(Duration::from_secs(1)))
        .unwrap();
    assert!(events.iter().any(|event| event.token() == listener_token));
    let (mut stream, _) = listener.accept().unwrap();
    let stream_token = Token(IO_TOKEN_START + 1);
    reactor
        .register(&mut stream, stream_token, Interest::READABLE)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    let mut request = [0; 4];
    loop {
        match stream.read_exact(&mut request) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                reactor
                    .poll(&mut events, Some(Duration::from_millis(50)))
                    .unwrap();
            }
            Err(error) => panic!("echo read failed: {error}"),
        }
    }
    assert_eq!(&request, b"ping");
    stream.write_all(&request).unwrap();
    assert_eq!(client.join().unwrap(), *b"ping");
}

#[test]
fn timer_expiring_during_poll_precedes_deferred_drain() {
    let mut event_loop = Loop::new().unwrap();
    let root = event_loop.root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let timer_order = Arc::clone(&order);
    event_loop.timers().insert(
        Instant::now() + Duration::from_millis(10),
        TimerEntry::once(move || timer_order.lock().unwrap().push("timer")),
    );

    let address = "127.0.0.1:0".to_socket_addrs().unwrap().next().unwrap();
    let mut listener = TcpListener::bind(address).unwrap();
    let address = listener.local_addr().unwrap();
    let token = Token(IO_TOKEN_START);
    event_loop
        .reactor()
        .register(&mut listener, token, Interest::READABLE)
        .unwrap();
    let readiness_order = Arc::clone(&order);
    event_loop
        .on_readiness(token, move |_, queues| {
            readiness_order.lock().unwrap().push("io");
            thread::sleep(Duration::from_millis(20));
            let deferred_order = Arc::clone(&readiness_order);
            queues
                .put(
                    root,
                    Event::callback(move || deferred_order.lock().unwrap().push("deferred")),
                )
                .map(|()| DrainState::Drained)
        })
        .unwrap();
    let client = StdTcpStream::connect(address).unwrap();

    event_loop.run_once(Some(Duration::from_secs(1))).unwrap();
    drop(client);
    assert_eq!(*order.lock().unwrap(), vec!["io", "timer", "deferred"]);
}

#[test]
fn removing_child_purges_descendant_links_from_root() {
    let mut queues = MultiQueue::new();
    let root = queues.root();
    let child = queues.child(root).unwrap();
    let nested = queues.child(child).unwrap();
    queues.put(child, Event::Signal(1)).unwrap();
    queues.put(nested, Event::Signal(2)).unwrap();

    queues.remove_owner(child).unwrap();

    assert!(queues.is_empty(root).unwrap());
    assert!(queues.len(child).is_err());
    assert!(queues.len(nested).is_err());
}

#[test]
fn partial_read_per_callback_still_drains_edgetriggered_stream() {
    // A readiness callback that consumes only one byte per invocation still
    // receives the whole payload: the pump re-invokes it until WouldBlock, so
    // under EPOLLET a partial read cannot strand buffered data.
    let mut event_loop = Loop::new().unwrap();
    let address = "127.0.0.1:0".to_socket_addrs().unwrap().next().unwrap();
    let listener = TcpListener::bind(address).unwrap();
    let address = listener.local_addr().unwrap();
    // The accepted stream is moved into the readiness callback; it is dropped
    // when `event_loop` (and its callback map) drop at the end of the test.

    let payload: Vec<u8> = (0..8192u16).map(|i| (i % 251) as u8).collect();
    let payload_for_client = payload.clone();
    let peer_done = Arc::new(AtomicBool::new(false));
    let client_done = Arc::clone(&peer_done);
    let client = thread::spawn(move || {
        let mut stream = StdTcpStream::connect(address).unwrap();
        stream.write_all(&payload_for_client).unwrap();
        while !client_done.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
    });

    // Accept synchronously; loopback connect completes almost immediately.
    let accept_deadline = Instant::now() + Duration::from_secs(2);
    let (mut server_stream, _) = loop {
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < accept_deadline, "listener accept timed out");
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };

    let stream_token = Token(IO_TOKEN_START);
    event_loop
        .reactor()
        .register(&mut server_stream, stream_token, Interest::READABLE)
        .unwrap();

    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    let callback_received = Arc::clone(&received);
    event_loop
        .on_readiness(stream_token, move |_, _queues| {
            let mut buf = [0u8; 1];
            match server_stream.read(&mut buf) {
                Ok(0) => Ok(DrainState::Drained), // EOF; nothing more to consume.
                Ok(n) => {
                    callback_received.lock().unwrap().extend_from_slice(&buf[..n]);
                    // Deliberately stop after one byte: the pump must re-invoke
                    // us until WouldBlock, proving partial reads cannot strand.
                    Ok(DrainState::KeepDraining)
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    Ok(DrainState::Drained)
                }
                Err(error) => Err(error.into()),
            }
        })
        .unwrap();

    let drain_deadline = Instant::now() + Duration::from_secs(5);
    while received.lock().unwrap().len() < payload.len() {
        assert!(
            Instant::now() < drain_deadline,
            "readiness callback never drained the full payload"
        );
        event_loop
            .run_once(Some(Duration::from_millis(50)))
            .unwrap();
    }

    assert_eq!(*received.lock().unwrap(), payload);
    peer_done.store(true, Ordering::Release);
    client.join().unwrap();
}

#[test]
fn stop_from_another_thread_terminates_running_loop() {
    let mut event_loop = Loop::new().unwrap();
    let stop_handle = event_loop.stop_handle();
    let stopper = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        stop_handle.stop();
    });

    let started = Instant::now();
    event_loop.run().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "loop did not terminate promptly after StopHandle::stop"
    );
    stopper.join().unwrap();
}

#[test]
fn stop_before_run_makes_run_return_immediately() {
    let mut event_loop = Loop::new().unwrap();
    let stop_handle = event_loop.stop_handle();
    stop_handle.stop();

    let started = Instant::now();
    event_loop.run().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "run should return immediately when stopped before entry"
    );
}
