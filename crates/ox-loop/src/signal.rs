use crate::{Event, MultiQueue, Owner, Reactor, Result};

#[cfg(unix)]
mod platform {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    use mio::Interest;
    use mio::unix::SourceFd;
    use signal_hook::iterator::backend::SignalDelivery;
    use signal_hook::iterator::exfiltrator::SignalOnly;

    use super::*;
    use crate::SIGNAL_TOKEN;

    /// Signal-hook self-pipe integrated as a normal mio readiness source.
    pub struct Signals {
        delivery: SignalDelivery<UnixStream, SignalOnly>,
    }

    impl Signals {
        /// Creates signal delivery for the supplied platform signal numbers.
        pub fn new(signal_numbers: &[i32]) -> Result<Self> {
            if let Some(signal) = signal_numbers.iter().copied().find(|signal| {
                *signal <= 0
                    || *signal >= 128
                    || signal_hook::consts::FORBIDDEN.contains(signal)
            }) {
                return Err(crate::Error::InvalidSignal(signal));
            }
            let (read, write) = UnixStream::pair()?;
            let delivery = SignalDelivery::with_pipe(
                read,
                write,
                SignalOnly::default(),
                signal_numbers.iter().copied(),
            )?;
            Ok(Self { delivery })
        }

        /// Registers the signal self-pipe in the reactor's internal token range.
        pub fn register(&mut self, reactor: &Reactor) -> Result<()> {
            // SourceFd is only the Unix registration adapter; Signals owns the
            // pipe. A future Windows adapter can keep this interface unchanged.
            let fd = self.delivery.get_read().as_raw_fd();
            let mut source = SourceFd(&fd);
            reactor.register_internal(&mut source, SIGNAL_TOKEN, Interest::READABLE)
        }

        /// Moves all coalesced pending signals into `owner`'s event queue.
        pub fn drain(&mut self, events: &mut MultiQueue, owner: Owner) -> Result<()> {
            for signal in self.delivery.pending() {
                events.put(owner, Event::Signal(signal))?;
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;

    /// Portable seam for a future Windows signal/event adapter.
    pub struct Signals;

    impl Signals {
        /// Reports that this platform has no signal adapter yet.
        pub fn new(_signal_numbers: &[i32]) -> Result<Self> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "signal delivery is not available on this platform",
            )
            .into())
        }

        /// No-op registration for the portable stub.
        pub fn register(&mut self, _reactor: &Reactor) -> Result<()> {
            Ok(())
        }

        /// No-op drain for the portable stub.
        pub fn drain(&mut self, _events: &mut MultiQueue, _owner: Owner) -> Result<()> {
            Ok(())
        }
    }
}

pub use platform::Signals;
