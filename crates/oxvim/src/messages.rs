//! Message output for the process paths that own the terminal streams.
//!
//! `message.c` `msg_puts_printf` (line 3019) writes message text to stderr,
//! or to stdout while `info_message` is set (line 3047), and drops it
//! entirely while `silent_mode` is set and `'verbose'` is zero (line 3038).
//!
//! Newlines separate messages rather than terminate them: `msg_start`
//! (`message.c` line 1770) emits the newline before the next message, in the
//! stream the previous message used, which is why `nvim --headless` running
//! two `:echo` commands writes `A\nB` with no trailing newline. Batch mode is
//! the exception: `print_line` (`ex_cmds.c` line 1721) and `do_set`
//! (`option.c` line 1680) add a newline after their output because a "batch
//! mode message should always end in newline".

use std::io::{self, Write};

use ox_editor::{Message, MessageDestination, MessageRouting};
use ox_types::Object;

/// The stream one message was written to.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Stream {
    Out,
    Err,
}

/// Writes messages to stdout and stderr with upstream's separation.
#[derive(Default)]
pub struct PrintfSink {
    /// Stream of the last message written, upstream's `msg_didout` plus the
    /// `info_message` state that decided where that text went.
    last: Option<Stream>,
}

impl PrintfSink {
    /// Writes one message to `destination`, or drops it when the sink decided
    /// the text is suppressed or belongs to a UI.
    pub fn write(&mut self, destination: MessageDestination, message: &Message) -> io::Result<()> {
        let stream = match destination {
            MessageDestination::Stdout => Stream::Out,
            MessageDestination::Stderr => Stream::Err,
            // A UI renders its own messages, and a suppressed message was
            // retained only so capture could read it.
            MessageDestination::Ui | MessageDestination::Suppressed => return Ok(()),
        };
        if let Some(previous) = self.last {
            self.put(previous, b"\n")?;
        }
        match &message.content {
            Object::String(text) => self.put(stream, text.as_bytes())?,
            value => self.put(stream, format!("{value:?}").as_bytes())?,
        }
        self.last = Some(stream);
        Ok(())
    }

    /// Ends the message stream, adding batch mode's trailing newline.
    ///
    /// `silent_mode` output always ends in a newline (`ex_cmds.c` line 1721),
    /// while `--headless` output does not.
    pub fn finish(&mut self, routing: MessageRouting) -> io::Result<()> {
        if let Some(stream) = self.last.take() {
            if routing.silent {
                self.put(stream, b"\n")?;
            }
        }
        Ok(())
    }

    fn put(&self, stream: Stream, bytes: &[u8]) -> io::Result<()> {
        match stream {
            Stream::Out => {
                let stdout = io::stdout();
                let mut handle = stdout.lock();
                handle.write_all(bytes)?;
                handle.flush()
            }
            Stream::Err => {
                let stderr = io::stderr();
                let mut handle = stderr.lock();
                handle.write_all(bytes)?;
                handle.flush()
            }
        }
    }
}
