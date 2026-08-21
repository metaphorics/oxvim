use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

#[test]
fn run_modes_respect_liveness_stop_and_unref() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let fired = Rc::new(Cell::new(0));
    let timer = Timer::new(&mut uv_loop).expect("timer");
    let callback_fired = Rc::clone(&fired);
    timer
        .start(&mut uv_loop, 0, 0, move |_, _| {
            callback_fired.set(callback_fired.get() + 1);
            Ok(())
        })
        .expect("start");
    timer.unref(&mut uv_loop).expect("unref");

    assert!(!uv_loop.loop_alive());
    assert!(!uv_loop.run_nowait().expect("nowait"));
    assert_eq!(fired.get(), 0);

    timer.ref_(&mut uv_loop).expect("ref");
    assert!(!uv_loop.run_once().expect("once"));
    assert_eq!(fired.get(), 1);

    let repeating = Timer::new(&mut uv_loop).expect("repeating timer");
    repeating
        .start(&mut uv_loop, 0, 1, move |event_loop, _| {
            assert_eq!(event_loop.loop_mode(), Some(RunMode::Default));
            event_loop.stop();
            Ok(())
        })
        .expect("start repeating");
    assert!(uv_loop.run_default().expect("default stopped with live timer"));
    repeating.stop(&mut uv_loop).expect("stop repeating");
    assert!(!uv_loop.loop_alive());
}

#[test]
fn stop_before_run_still_fires_due_timer_once() {
    // Per luvref.txt:464-471 a stop requested before run ends the loop no
    // sooner than one forced-nowait iteration, so a due timer still fires.
    let mut uv_loop = UvLoop::new().expect("loop");
    let fired = Rc::new(Cell::new(0));
    let timer = Timer::new(&mut uv_loop).expect("timer");
    let callback_fired = Rc::clone(&fired);
    timer
        .start(&mut uv_loop, 1, 0, move |_, _| {
            callback_fired.set(callback_fired.get() + 1);
            Ok(())
        })
        .expect("start");
    thread::sleep(Duration::from_millis(5)); // deadline has passed; timer is due

    uv_loop.stop();
    assert!(!uv_loop.run_default().expect("default honoring pending stop"));
    assert_eq!(fired.get(), 1, "due timer fired exactly once");

    // run_once honors the same pending-stop contract.
    let mut uv_loop = UvLoop::new().expect("loop");
    let once_fired = Rc::new(Cell::new(0));
    let timer = Timer::new(&mut uv_loop).expect("timer");
    let once_callback = Rc::clone(&once_fired);
    timer
        .start(&mut uv_loop, 1, 0, move |_, _| {
            once_callback.set(once_callback.get() + 1);
            Ok(())
        })
        .expect("start");
    thread::sleep(Duration::from_millis(5));

    uv_loop.stop();
    assert!(!uv_loop.run_once().expect("once honoring pending stop"));
    assert_eq!(once_fired.get(), 1, "due timer fired exactly once");
}

#[cfg(unix)]
#[test]
fn signal_driver_does_not_consume_public_io_token() {
    use std::os::unix::net::UnixStream;

    use mio::{Interest, Token};
    use ox_loop::DrainState;

    // Constructing the loop installs the signal driver, whose self-pipe now
    // lives on ox-loop's internal token range. The first public I/O source at
    // IO_TOKEN_START must register without a DuplicateReadiness collision.
    let mut uv_loop = UvLoop::new().expect("loop");
    let (read, _write) = UnixStream::pair().expect("socket pair");
    read.set_nonblocking(true).expect("nonblocking");
    let mut source = mio::net::UnixStream::from_std(read);
    let token = Token(ox_loop::IO_TOKEN_START);

    uv_loop
        .inner()
        .reactor()
        .register(&mut source, token, Interest::READABLE)
        .expect("public source must register with the signal driver installed");
    uv_loop
        .inner_mut()
        .on_readiness(token, |_, _| Ok(DrainState::Drained))
        .expect("public readiness token must not collide with the signal pipe");
}

#[test]
fn timer_repeat_again_and_repeat_mutation_follow_luv_cadence() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let timer = Timer::new(&mut uv_loop).expect("timer");
    let timer_copy = timer;
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    timer
        .start(&mut uv_loop, 0, 50, move |event_loop, _| {
            let next = callback_calls.get() + 1;
            callback_calls.set(next);
            if next == 1 {
                timer_copy.set_repeat(event_loop, 1)?;
            } else {
                timer_copy.stop(event_loop)?;
            }
            Ok(())
        })
        .expect("start");

    assert!(!uv_loop.run_default().expect("default"));
    assert_eq!(calls.get(), 2);
    assert_eq!(timer.get_repeat(&uv_loop).expect("repeat"), 1);

    timer.stop(&mut uv_loop).expect("stop");
    let before = Instant::now();
    timer.again(&mut uv_loop).expect("again");
    assert!(!uv_loop.run_default().expect("again run"));
    assert!(before.elapsed() >= Duration::from_millis(1));
    assert_eq!(calls.get(), 3);
}

#[cfg(unix)]
#[test]
fn prepare_io_and_check_follow_documented_order() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use mio::{Interest, Token};
    use ox_loop::DrainState;

    let mut uv_loop = UvLoop::new().expect("loop");
    let order = Rc::new(RefCell::new(Vec::new()));
    let prepare = Prepare::new(&mut uv_loop).expect("prepare");
    let check = Check::new(&mut uv_loop).expect("check");

    let prepare_order = Rc::clone(&order);
    prepare
        .start(&mut uv_loop, move |_, _| {
            prepare_order.borrow_mut().push("prepare");
            Ok(())
        })
        .expect("prepare start");
    let check_order = Rc::clone(&order);
    check
        .start(&mut uv_loop, move |_, _| {
            check_order.borrow_mut().push("check");
            Ok(())
        })
        .expect("check start");

    let (read, mut write) = UnixStream::pair().expect("socket pair");
    read.set_nonblocking(true).expect("nonblocking");
    let mut read = mio::net::UnixStream::from_std(read);
    let token = Token(ox_loop::IO_TOKEN_START + 1);
    uv_loop
        .inner()
        .reactor()
        .register(&mut read, token, Interest::READABLE)
        .expect("register");
    let io_order = Rc::clone(&order);
    uv_loop
        .inner_mut()
        .on_readiness(token, move |_, _| {
            let mut byte = [0_u8; 1];
            match read.read(&mut byte) {
                Ok(0) => Ok(DrainState::Drained),
                Ok(_) => {
                    io_order.borrow_mut().push("io");
                    Ok(DrainState::KeepDraining)
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    Ok(DrainState::Drained)
                }
                Err(error) => Err(error.into()),
            }
        })
        .expect("readiness callback");
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        write.write_all(&[1]).expect("write");
    });

    assert!(uv_loop.run_once().expect("turn remains live"));
    writer.join().expect("writer");
    assert_eq!(&*order.borrow(), &["prepare", "io", "check"]);
    prepare.stop(&mut uv_loop).expect("prepare stop");
    check.stop(&mut uv_loop).expect("check stop");
}

#[test]
fn unreferenced_idle_spins_until_referenced_timer_finishes() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let idle = Idle::new(&mut uv_loop).expect("idle");
    let iterations = Rc::new(Cell::new(0));
    let callback_iterations = Rc::clone(&iterations);
    idle.start(&mut uv_loop, move |_, _| {
        callback_iterations.set(callback_iterations.get() + 1);
        Ok(())
    })
    .expect("idle start");
    idle.unref(&mut uv_loop).expect("idle unref");

    let timer = Timer::new(&mut uv_loop).expect("timer");
    timer
        .start(&mut uv_loop, 2, 0, |_, _| Ok(()))
        .expect("timer start");
    assert!(!uv_loop.run_default().expect("default"));
    assert!(iterations.get() > 1);
    assert!(idle.is_active(&uv_loop));
}

#[test]
fn async_cross_thread_wakes_and_coalesces() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    let slot = Rc::new(Cell::new(None::<Async>));
    let callback_slot = Rc::clone(&slot);
    let async_handle = Async::new(&mut uv_loop, move |event_loop, _| {
        callback_calls.set(callback_calls.get() + 1);
        callback_slot.get().expect("installed handle").close(event_loop)?;
        Ok(())
    })
    .expect("async");
    slot.set(Some(async_handle));
    let sender = async_handle.sender(&uv_loop).expect("sender");

    let thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(5));
        for _ in 0..10 {
            sender.send().expect("send");
        }
    });
    assert!(!uv_loop.run_default().expect("default"));
    thread.join().expect("sender thread");
    assert!((1..=10).contains(&calls.get()));
}

#[cfg(unix)]
#[test]
fn signal_oneshot_delivers_one_raise() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let signal = Signal::new(&mut uv_loop).expect("signal");
    let calls = Rc::new(Cell::new(0));
    let callback_calls = Rc::clone(&calls);
    signal
        .start_oneshot(
            &mut uv_loop,
            rustix::process::Signal::USR1.as_raw(),
            move |_, _, signum| {
                assert_eq!(signum, rustix::process::Signal::USR1.as_raw());
                callback_calls.set(callback_calls.get() + 1);
                Ok(())
            },
        )
        .expect("signal start");

    let raiser = thread::spawn(|| {
        thread::sleep(Duration::from_millis(5));
        rustix::process::kill_process(
            rustix::process::getpid(),
            rustix::process::Signal::USR1,
        )
        .expect("raise SIGUSR1");
    });
    assert!(!uv_loop.run_default().expect("default"));
    raiser.join().expect("raiser");
    assert_eq!(calls.get(), 1);
    assert!(!signal.is_active(&uv_loop));
}

#[test]
fn callbacks_can_close_and_create_handles_without_reentrant_borrows() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let events = Rc::new(RefCell::new(Vec::new()));
    let victim = Idle::new(&mut uv_loop).expect("victim");
    victim
        .start(&mut uv_loop, |_, _| Ok(()))
        .expect("victim start");

    let starter = Timer::new(&mut uv_loop).expect("starter");
    let timer_events = Rc::clone(&events);
    let close_events = Rc::clone(&events);
    starter
        .start(&mut uv_loop, 0, 0, move |event_loop, _| {
            timer_events.borrow_mut().push("starter");
            let callback_events = Rc::clone(&close_events);
            victim.close_with(event_loop, move |_, _| {
                callback_events.borrow_mut().push("closed");
                Ok(())
            })?;
            let created = Timer::new(event_loop)?;
            let created_events = Rc::clone(&timer_events);
            created.start(event_loop, 0, 0, move |_, _| {
                created_events.borrow_mut().push("created");
                Ok(())
            })?;
            Ok(())
        })
        .expect("starter start");

    assert!(!uv_loop.run_default().expect("default"));
    assert_eq!(&*events.borrow(), &["starter", "created", "closed"]);
}

#[test]
fn reentrant_stop_suppresses_snapshotted_callbacks() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let stopper = Timer::new(&mut uv_loop).expect("stopper");
    let victim_timer = Timer::new(&mut uv_loop).expect("victim timer");
    let victim_idle = Idle::new(&mut uv_loop).expect("victim idle");
    let timer_calls = Rc::new(Cell::new(0));
    let idle_calls = Rc::new(Cell::new(0));
    let timer_counter = Rc::clone(&timer_calls);
    victim_timer
        .start(&mut uv_loop, 0, 0, move |_, _| {
            timer_counter.set(timer_counter.get() + 1);
            Ok(())
        })
        .expect("victim timer start");
    let idle_counter = Rc::clone(&idle_calls);
    victim_idle
        .start(&mut uv_loop, move |_, _| {
            idle_counter.set(idle_counter.get() + 1);
            Ok(())
        })
        .expect("victim idle start");

    stopper
        .start(&mut uv_loop, 0, 0, move |event_loop, _| {
            victim_timer.stop(event_loop)?;
            victim_idle.stop(event_loop)?;
            Ok(())
        })
        .expect("stopper start");

    assert!(!uv_loop.run_default().expect("default"));
    assert_eq!(timer_calls.get(), 0);
    assert_eq!(idle_calls.get(), 0);
}

#[test]
fn callback_failures_and_panics_become_error_events() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let failed = Timer::new(&mut uv_loop).expect("failed timer");
    failed
        .start(&mut uv_loop, 0, 0, |_, _| {
            Err(CallbackError::new("binding failure"))
        })
        .expect("failed start");
    let panicked = Timer::new(&mut uv_loop).expect("panic timer");
    panicked
        .start(&mut uv_loop, 0, 0, |_, _| -> Result<(), CallbackError> {
            panic!("callback panic")
        })
        .expect("panic start");

    assert!(!uv_loop.run_default().expect("default"));
    let first = uv_loop.pop_callback_error().expect("returned error");
    let second = uv_loop.pop_callback_error().expect("panic error");
    assert_eq!(first.phase, CallbackPhase::Timer);
    assert_eq!(first.error, CallbackError::new("binding failure"));
    assert_eq!(second.phase, CallbackPhase::Timer);
    assert_eq!(second.error, CallbackError::Panic("callback panic".to_owned()));
}

#[test]
fn walk_sees_closing_handles_until_the_next_turn() {
    let mut uv_loop = UvLoop::new().expect("loop");
    let timer = Timer::new(&mut uv_loop).expect("timer");
    timer.close(&mut uv_loop).expect("close");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let walked = Rc::clone(&seen);
    uv_loop.walk(move |event_loop, id| {
        walked.borrow_mut().push((id, event_loop.is_closing(id)));
    });
    assert_eq!(&*seen.borrow(), &[(timer.id(), true)]);
    assert!(!uv_loop.run_nowait().expect("close turn"));
}

#[test]
fn stop_raised_during_forced_turn_is_consumed() {
    // A stop requested before run executes one forced-nowait iteration.
    // If a callback calls stop() during that forced turn, the flag must be
    // consumed along with the pre-run stop so the next run behaves normally.
    let mut uv_loop = UvLoop::new().expect("loop");
    let forced_fired = Rc::new(Cell::new(false));
    let forced_copy = Rc::clone(&forced_fired);
    let stopper = Timer::new(&mut uv_loop).expect("stopper");
    stopper
        .start(&mut uv_loop, 0, 0, move |event_loop, _| {
            forced_copy.set(true);
            event_loop.stop();
            Ok(())
        })
        .expect("start stopper");

    uv_loop.stop();
    assert!(!uv_loop.run_default().expect("forced turn consumes pending stop"));
    assert!(forced_fired.get());

    // A fresh repeating timer must run normally afterward, not exit after one turn.
    let calls = Rc::new(Cell::new(0));
    let timer = Timer::new(&mut uv_loop).expect("timer");
    let timer_copy = timer;
    let callback_calls = Rc::clone(&calls);
    timer
        .start(&mut uv_loop, 0, 1, move |event_loop, _| {
            let next = callback_calls.get() + 1;
            callback_calls.set(next);
            if next >= 3 {
                timer_copy.stop(event_loop)?;
            }
            Ok(())
        })
        .expect("start repeating timer");

    assert!(!uv_loop.run_default().expect("default should run to completion"));
    assert_eq!(calls.get(), 3, "repeating timer must fire three times");
}

#[test]
fn misc_time_and_directory_contracts_are_sane() {
    let first = misc::hrtime();
    let second = misc::hrtime();
    assert!(second >= first);
    let (seconds, micros) = misc::gettimeofday().expect("wall time");
    assert!(seconds > 1_600_000_000);
    assert!(micros < 1_000_000);

    let original = misc::cwd().expect("cwd");
    let temporary = misc::os_tmpdir();
    misc::chdir(&temporary).expect("chdir temporary");
    assert_eq!(misc::cwd().expect("temporary cwd"), temporary);
    misc::chdir(&original).expect("restore cwd");
    assert_eq!(misc::cwd().expect("restored cwd"), original);

    let uname = misc::os_uname();
    assert!(!uname.sysname.is_empty());
    assert!(misc::getpid() > 0);

    let mut uv_loop = UvLoop::new().expect("loop");
    let cached = uv_loop.now();
    thread::sleep(Duration::from_millis(1));
    assert_eq!(uv_loop.now(), cached);
    uv_loop.update_time();
    assert!(uv_loop.now() >= cached);
}
