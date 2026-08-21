//! Unix-only `SCM_RIGHTS` file-descriptor passing over connected sockets.
//!
//! These helpers implement the IPC half of `uv.write2()` (sending a stream
//! handle across a named pipe) and the `uv.pipe_pending_*()` receive track in
//! `runtime/doc/luvref.txt`. They wrap `rustix::net::sendmsg`/`recvmsg` with
//! an `ScmRights` ancillary message and are only compiled on Unix, which is
//! where libuv's IPC fd-passing works; Windows returns `EAGAIN` for
//! `try_write2` in libuv and is out of scope here.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

/// Writes `data` to `socket`, additionally passing `send_fd` as an
/// `SCM_RIGHTS` ancillary message in the same message.
///
/// This is the ox-uv implementation of the fd-carrying variant of
/// `uv.write2()` in `runtime/doc/luvref.txt`. It performs a single synchronous
/// `sendmsg`, so the caller must be prepared for `WouldBlock` on a full socket
/// buffer (matching libuv's note that a user should handle `EAGAIN`).
pub(crate) fn send_handle<Fd: AsFd, Rhs: AsFd>(
    socket: Fd,
    data: &[u8],
    send_fd: &Rhs,
) -> io::Result<usize> {
    let iov = [IoSlice::new(data)];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let rights = [send_fd.as_fd()];
    control.push(SendAncillaryMessage::ScmRights(&rights));
    sendmsg(socket, &iov, &mut control, SendFlags::empty())
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))
}

/// Receives one message from `socket`, returning its payload and any
/// `SCM_RIGHTS` descriptor it carried.
///
/// The received descriptor is owned and returned for the caller to publish as
/// a pending handle. End-of-file is indicated by an empty payload with no
/// descriptor.
pub(crate) fn recv_handle<Fd: AsFd>(
    socket: Fd,
    max: usize,
) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let mut payload = vec![0u8; max];
    let mut iov = [IoSliceMut::new(&mut payload)];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let message = recvmsg(socket, &mut iov, &mut control, RecvFlags::empty())
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    payload.truncate(message.bytes);
    let mut received = None;
    for ancillary in control.drain() {
        if let RecvAncillaryMessage::ScmRights(mut rights) = ancillary {
            if let Some(fd) = rights.next() {
                received = Some(fd);
                break;
            }
        }
    }
    Ok((payload, received))
}