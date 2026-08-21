//! Synchronous filesystem operations and pool-backed asynchronous dispatch.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(not(any(unix, windows)))]
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::UvLoop;
use crate::pool::{LoopPoster, Pool, PoolError};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Filesystem failure with a libuv-style errno name.
#[derive(Debug, thiserror::Error)]
#[error("{name}: {message}")]
pub struct FsError {
    /// Stable POSIX/libuv-style error name when one is known.
    pub name: &'static str,
    /// Platform error message.
    pub message: String,
    /// Original platform errno, when available.
    pub raw_os_error: Option<i32>,
}

impl FsError {
    fn from_io(error: io::Error) -> Self {
        let raw_os_error = error.raw_os_error();
        Self {
            name: errno_name(raw_os_error, error.kind()),
            message: error.to_string(),
            raw_os_error,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self { name: "EINVAL", message: message.into(), raw_os_error: None }
    }

    fn pool(error: PoolError) -> Self {
        Self { name: "ECANCELED", message: error.to_string(), raw_os_error: None }
    }
}

impl From<io::Error> for FsError {
    fn from(error: io::Error) -> Self { Self::from_io(error) }
}

/// Result returned by filesystem operations.
pub type FsResult<T> = Result<T, FsError>;

/// Cloneable owner for an open file descriptor.
#[derive(Clone, Debug)]
pub struct FileHandle(Arc<Mutex<Option<File>>>);

impl FileHandle {
    fn new(file: File) -> Self { Self(Arc::new(Mutex::new(Some(file)))) }

    fn with_file<T>(&self, operation: impl FnOnce(&File) -> io::Result<T>) -> FsResult<T> {
        let guard = self.0.lock().map_err(|_| FsError::invalid("file handle lock is poisoned"))?;
        let file = guard.as_ref().ok_or_else(|| FsError::invalid("file handle is closed"))?;
        operation(file).map_err(FsError::from_io)
    }
}

/// Portable subset of open options accepted by `fs_open`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenFlags {
    /// Allow reading.
    pub read: bool,
    /// Allow writing.
    pub write: bool,
    /// Append on every write.
    pub append: bool,
    /// Truncate on open.
    pub truncate: bool,
    /// Create the file if missing.
    pub create: bool,
    /// Fail if the file already exists.
    pub create_new: bool,
}

impl OpenFlags {
    /// Open for reading only.
    pub const READ: Self = Self { read: true, write: false, append: false, truncate: false, create: false, create_new: false };
    /// Open for writing (and truncating/creating) only.
    pub const WRITE: Self = Self { read: false, write: true, append: false, truncate: true, create: true, create_new: false };
    /// Open for both reading and writing.
    pub const READ_WRITE: Self = Self { read: true, write: true, append: false, truncate: false, create: false, create_new: false };
}

/// One filesystem timestamp as seconds and nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FsTime {
    /// Seconds since the Unix epoch.
    pub sec: i64,
    /// Nanoseconds past `sec`.
    pub nsec: u32,
}

/// Metadata snapshot corresponding to luv's `uv.fs_stat` table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stat {
    /// Device ID of the filesystem.
    pub dev: u64,
    /// File mode and permissions.
    pub mode: u32,
    /// Number of hard links.
    pub nlink: u64,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
    /// Device ID for special files.
    pub rdev: u64,
    /// Inode number.
    pub ino: u64,
    /// File size in bytes.
    pub size: u64,
    /// Preferred block size.
    pub blksize: u64,
    /// Number of 512-byte blocks allocated.
    pub blocks: u64,
    /// BSD file flags.
    pub flags: u64,
    /// Inode generation.
    pub r#gen: u64,
    /// Last access time.
    pub atime: FsTime,
    /// Last modification time.
    pub mtime: FsTime,
    /// Last status change time.
    pub ctime: FsTime,
    /// Creation/birth time.
    pub birthtime: FsTime,
}

/// Portable filesystem capacity snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatFs {
    /// Filesystem type.
    pub kind: u64,
    /// Fundamental block size.
    pub block_size: u64,
    /// Total blocks.
    pub blocks: u64,
    /// Free blocks available to all users.
    pub blocks_free: u64,
    /// Free blocks available to non-superusers.
    pub blocks_available: u64,
    /// Total file nodes.
    pub files: u64,
    /// Free file nodes.
    pub files_free: u64,
}

/// Directory entry kind exposed by scandir/readdir.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirEntryType {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Named pipe.
    Fifo,
    /// Socket.
    Socket,
    /// Character device.
    Character,
    /// Block device.
    Block,
    /// Unknown or unclassified.
    Unknown,
}

/// A directory entry without an implicit metadata lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// Entry name.
    pub name: String,
    /// Entry kind without an implicit metadata lookup.
    pub kind: DirEntryType,
}

/// Sorted synchronous scandir iterator.
pub struct Scandir { entries: std::vec::IntoIter<DirEntry> }

impl Iterator for Scandir {
    type Item = DirEntry;
    fn next(&mut self) -> Option<Self::Item> { self.entries.next() }
}

/// Stateful directory stream used by opendir/readdir/closedir.
#[derive(Debug)]
pub struct Directory { entries: Vec<DirEntry>, cursor: usize, batch_size: usize, closed: bool }

/// Opens a file. See `uv.fs_open()` in `runtime/doc/luvref.txt`.
pub fn open(path: impl AsRef<Path>, flags: OpenFlags, mode: u32) -> FsResult<FileHandle> {
    let mut options = OpenOptions::new();
    options.read(flags.read).write(flags.write).append(flags.append)
        .truncate(flags.truncate).create(flags.create).create_new(flags.create_new);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(mode); }
    let file = options.open(path).map_err(FsError::from_io)?;
    Ok(FileHandle::new(file))
}

/// Closes a file. See `uv.fs_close()` in `runtime/doc/luvref.txt`.
pub fn close(handle: &FileHandle) -> FsResult<()> {
    let mut guard = handle.0.lock().map_err(|_| FsError::invalid("file handle lock is poisoned"))?;
    guard.take().ok_or_else(|| FsError::invalid("file handle is already closed"))?;
    Ok(())
}

/// Reads bytes, optionally at a position. See `uv.fs_read()` in `runtime/doc/luvref.txt`.
pub fn read(handle: &FileHandle, size: usize, offset: Option<u64>) -> FsResult<Vec<u8>> {
    handle.with_file(|file| {
        let mut data = vec![0; size];
        let count = match offset {
            Some(offset) => read_at(file, &mut data, offset)?,
            None => (&*file).read(&mut data)?,
        };
        data.truncate(count);
        Ok(data)
    })
}

/// Writes bytes, optionally at a position. See `uv.fs_write()` in `runtime/doc/luvref.txt`.
pub fn write(handle: &FileHandle, data: &[u8], offset: Option<u64>) -> FsResult<usize> {
    handle.with_file(|file| match offset { Some(offset) => write_at(file, data, offset), None => (&*file).write(data) })
}

/// Reads bytes into multiple buffers with a single `readv`/`preadv` call.
///
/// Each element of `sizes` is one destination buffer; the returned vector has
/// one entry per requested buffer, truncated to the number of bytes actually
/// read into it. This is the table-of-buffers form of `uv.fs_read()` in
/// `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn readv(handle: &FileHandle, sizes: &[usize], offset: Option<u64>) -> FsResult<Vec<Vec<u8>>> {
    use std::io::{Error as IoError, IoSliceMut};
    use std::os::fd::AsFd as _;

    handle.with_file(|file| {
        let fd = file.as_fd();
        let mut buffers: Vec<Vec<u8>> = sizes.iter().map(|size| vec![0u8; *size]).collect();
        let mut iovecs: Vec<IoSliceMut<'_>> = buffers.iter_mut().map(|buffer| IoSliceMut::new(buffer)).collect();
        let read_total: usize = match offset {
            Some(offset) => rustix::io::preadv(fd, &mut iovecs, offset),
            None => rustix::io::readv(fd, &mut iovecs),
        }
        .map_err(|error| IoError::from_raw_os_error(error.raw_os_error()))?;
        let mut remaining = read_total;
        for buffer in buffers.iter_mut() {
            if remaining == 0 {
                buffer.clear();
                continue;
            }
            let take = buffer.len().min(remaining);
            buffer.truncate(take);
            remaining -= take;
        }
        Ok(buffers)
    })
}

/// Sequential approximation of [`readv`] for platforms without `readv`/`preadv`.
#[cfg(not(unix))]
pub fn readv(handle: &FileHandle, sizes: &[usize], mut offset: Option<u64>) -> FsResult<Vec<Vec<u8>>> {
    let mut buffers = Vec::with_capacity(sizes.len());
    for size in sizes {
        let data = read(handle, *size, offset)?;
        offset = None;
        buffers.push(data);
    }
    Ok(buffers)
}

/// Writes multiple buffers with a single `writev`/`pwritev` call.
///
/// The buffers are emitted in order in one system call; the return value is
/// the total number of bytes written. This is the table-of-buffers form of
/// `uv.fs_write()` in `runtime/doc/luvref.txt`, whose `data` argument is a
/// `buffer` (a string or a sequential table of strings).
#[cfg(unix)]
pub fn writev(handle: &FileHandle, buffers: &[Vec<u8>], offset: Option<u64>) -> FsResult<usize> {
    use std::io::{Error as IoError, IoSlice};
    use std::os::fd::AsFd as _;

    handle.with_file(|file| {
        let fd = file.as_fd();
        let iovecs: Vec<IoSlice<'_>> = buffers.iter().map(|buffer| IoSlice::new(buffer.as_slice())).collect();
        match offset {
            Some(offset) => rustix::io::pwritev(fd, &iovecs, offset),
            None => rustix::io::writev(fd, &iovecs),
        }
        .map_err(|error| IoError::from_raw_os_error(error.raw_os_error()))
    })
}

/// Sequential approximation of [`writev`] for platforms without `writev`/`pwritev`.
#[cfg(not(unix))]
pub fn writev(handle: &FileHandle, buffers: &[Vec<u8>], mut offset: Option<u64>) -> FsResult<usize> {
    let mut total = 0usize;
    for buffer in buffers {
        total += write(handle, buffer, offset)?;
        offset = None;
    }
    Ok(total)
}

/// Creates a directory. See `uv.fs_mkdir()` in `runtime/doc/luvref.txt`.
pub fn mkdir(path: impl AsRef<Path>, mode: u32) -> FsResult<()> {
    fs::create_dir(&path).map_err(FsError::from_io)?;
    #[cfg(unix)] chmod(path, mode)?;
    Ok(())
}
/// Removes an empty directory. See `uv.fs_rmdir()` in `runtime/doc/luvref.txt`.
pub fn rmdir(path: impl AsRef<Path>) -> FsResult<()> { fs::remove_dir(path).map_err(FsError::from_io) }
/// Removes a file. See `uv.fs_unlink()` in `runtime/doc/luvref.txt`.
pub fn unlink(path: impl AsRef<Path>) -> FsResult<()> { fs::remove_file(path).map_err(FsError::from_io) }
/// Renames a path. See `uv.fs_rename()` in `runtime/doc/luvref.txt`.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> FsResult<()> { fs::rename(from, to).map_err(FsError::from_io) }
/// Returns followed metadata. See `uv.fs_stat()` in `runtime/doc/luvref.txt`.
pub fn stat(path: impl AsRef<Path>) -> FsResult<Stat> { fs::metadata(path).map(|m| stat_from_metadata(&m)).map_err(FsError::from_io) }
/// Returns link metadata. See `uv.fs_lstat()` in `runtime/doc/luvref.txt`.
pub fn lstat(path: impl AsRef<Path>) -> FsResult<Stat> { fs::symlink_metadata(path).map(|m| stat_from_metadata(&m)).map_err(FsError::from_io) }
/// Returns open-file metadata. See `uv.fs_fstat()` in `runtime/doc/luvref.txt`.
pub fn fstat(handle: &FileHandle) -> FsResult<Stat> { handle.with_file(|f| f.metadata().map(|m| stat_from_metadata(&m))) }
/// Creates a hard link. See `uv.fs_link()` in `runtime/doc/luvref.txt`.
pub fn link(existing: impl AsRef<Path>, new: impl AsRef<Path>) -> FsResult<()> { fs::hard_link(existing, new).map_err(FsError::from_io) }
/// Reads a symbolic link. See `uv.fs_readlink()` in `runtime/doc/luvref.txt`.
pub fn readlink(path: impl AsRef<Path>) -> FsResult<PathBuf> { fs::read_link(path).map_err(FsError::from_io) }
/// Resolves a canonical path. See `uv.fs_realpath()` in `runtime/doc/luvref.txt`.
pub fn realpath(path: impl AsRef<Path>) -> FsResult<PathBuf> { fs::canonicalize(path).map_err(FsError::from_io) }

/// Creates a symbolic link. See `uv.fs_symlink()` in `runtime/doc/luvref.txt`.
pub fn symlink(target: impl AsRef<Path>, link_path: impl AsRef<Path>, directory: bool) -> FsResult<()> {
    #[cfg(unix)] { let _ = directory; std::os::unix::fs::symlink(target, link_path).map_err(FsError::from_io) }
    #[cfg(windows)] { if directory { std::os::windows::fs::symlink_dir(target, link_path).map_err(FsError::from_io) } else { std::os::windows::fs::symlink_file(target, link_path).map_err(FsError::from_io) } }
    #[cfg(not(any(unix, windows)))] { let _ = (target, link_path, directory); Err(FsError { name: "ENOSYS", message: "symbolic links are unsupported".into(), raw_os_error: None }) }
}

/// Changes path permissions. See `uv.fs_chmod()` in `runtime/doc/luvref.txt`.
pub fn chmod(path: impl AsRef<Path>, mode: u32) -> FsResult<()> {
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(FsError::from_io) }
    #[cfg(not(unix))] { let _ = (path, mode); Err(FsError { name: "ENOSYS", message: "chmod is unsupported".into(), raw_os_error: None }) }
}
/// Changes file permissions. See `uv.fs_fchmod()` in `runtime/doc/luvref.txt`.
pub fn fchmod(handle: &FileHandle, mode: u32) -> FsResult<()> {
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; handle.with_file(|f| f.set_permissions(fs::Permissions::from_mode(mode))) }
    #[cfg(not(unix))] { let _ = (handle, mode); Err(FsError { name: "ENOSYS", message: "fchmod is unsupported".into(), raw_os_error: None }) }
}

/// Changes path ownership on Unix. See `uv.fs_chown()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn chown(path: impl AsRef<Path>, uid: Option<u32>, gid: Option<u32>) -> FsResult<()> {
    let (uid, gid) = ownership_ids(uid, gid)?;
    rustix::fs::chown(path.as_ref(), uid, gid).map_err(|e| FsError::from_io(io::Error::from_raw_os_error(e.raw_os_error())))
}
/// Changes open-file ownership on Unix. See `uv.fs_fchown()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn fchown(handle: &FileHandle, uid: Option<u32>, gid: Option<u32>) -> FsResult<()> {
    let (uid, gid) = ownership_ids(uid, gid)?;
    handle.with_file(|file| rustix::fs::fchown(file, uid, gid).map_err(|e| io::Error::from_raw_os_error(e.raw_os_error())))
}
/// Changes symlink ownership on Unix. See `uv.fs_lchown()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn lchown(path: impl AsRef<Path>, uid: Option<u32>, gid: Option<u32>) -> FsResult<()> {
    use rustix::fs::{AtFlags, CWD};
    let (uid, gid) = ownership_ids(uid, gid)?;
    rustix::fs::chownat(CWD, path.as_ref(), uid, gid, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| FsError::from_io(io::Error::from_raw_os_error(e.raw_os_error())))
}

/// Truncates a path. See `uv.fs_truncate()` in `runtime/doc/luvref.txt`.
pub fn truncate(path: impl AsRef<Path>, length: u64) -> FsResult<()> { OpenOptions::new().write(true).open(path).and_then(|f| f.set_len(length)).map_err(FsError::from_io) }
/// Truncates an open file. See `uv.fs_ftruncate()` in `runtime/doc/luvref.txt`.
pub fn ftruncate(handle: &FileHandle, length: u64) -> FsResult<()> { handle.with_file(|f| f.set_len(length)) }
/// Flushes file contents and metadata. See `uv.fs_fsync()` in `runtime/doc/luvref.txt`.
pub fn fsync(handle: &FileHandle) -> FsResult<()> { handle.with_file(File::sync_all) }
/// Flushes file contents. See `uv.fs_fdatasync()` in `runtime/doc/luvref.txt`.
pub fn fdatasync(handle: &FileHandle) -> FsResult<()> { handle.with_file(File::sync_data) }

/// Tests requested access bits. See `uv.fs_access()` in `runtime/doc/luvref.txt`.
pub fn access(path: impl AsRef<Path>, read: bool, write: bool, execute: bool) -> FsResult<bool> {
    #[cfg(unix)] {
        use rustix::fs::Access;
        let mut requested = Access::EXISTS;
        if read { requested |= Access::READ_OK; }
        if write { requested |= Access::WRITE_OK; }
        if execute { requested |= Access::EXEC_OK; }
        rustix::fs::access(path.as_ref(), requested).map_err(|error| FsError::from_io(io::Error::from_raw_os_error(error.raw_os_error())))?;
        Ok(true)
    }
    #[cfg(not(unix))] {
        let metadata = fs::metadata(path).map_err(FsError::from_io)?;
        let _ = (read, execute);
        if write && metadata.permissions().readonly() { Err(FsError { name: "EACCES", message: "path is read-only".into(), raw_os_error: None }) } else { Ok(true) }
    }
}

/// Returns filesystem capacity data. See `uv.fs_statfs()` in `runtime/doc/luvref.txt`.
pub fn statfs(path: impl AsRef<Path>) -> FsResult<StatFs> {
    #[cfg(unix)] {
        let value = rustix::fs::statfs(path.as_ref()).map_err(|e| FsError::from_io(io::Error::from_raw_os_error(e.raw_os_error())))?;
        Ok(StatFs { kind: value.f_type as u64, block_size: value.f_bsize as u64, blocks: value.f_blocks as u64, blocks_free: value.f_bfree as u64, blocks_available: value.f_bavail as u64, files: value.f_files as u64, files_free: value.f_ffree as u64 })
    }
    #[cfg(not(unix))] {
        let _ = path;
        Err(FsError { name: "ENOSYS", message: "statfs requires a safe platform backend".into(), raw_os_error: None })
    }
}

/// Creates a sorted directory iterator. See `uv.fs_scandir()` in `runtime/doc/luvref.txt`.
pub fn scandir(path: impl AsRef<Path>) -> FsResult<Scandir> { Ok(Scandir { entries: collect_entries(path)?.into_iter() }) }
/// Advances a scandir request. See `uv.fs_scandir_next()` in `runtime/doc/luvref.txt`.
pub fn scandir_next(scan: &mut Scandir) -> Option<DirEntry> { scan.next() }
/// Opens a directory stream. See `uv.fs_opendir()` in `runtime/doc/luvref.txt`.
pub fn opendir(path: impl AsRef<Path>, entries: usize) -> FsResult<Directory> { Ok(Directory { entries: collect_entries(path)?, cursor: 0, batch_size: entries.max(1), closed: false }) }
/// Reads the next directory batch. See `uv.fs_readdir()` in `runtime/doc/luvref.txt`.
pub fn readdir(directory: &mut Directory) -> FsResult<Vec<DirEntry>> {
    if directory.closed { return Err(FsError::invalid("directory handle is closed")); }
    let end = directory.cursor.saturating_add(directory.batch_size).min(directory.entries.len());
    let result = directory.entries[directory.cursor..end].to_vec(); directory.cursor = end; Ok(result)
}
/// Closes a directory stream. See `uv.fs_closedir()` in `runtime/doc/luvref.txt`.
pub fn closedir(directory: &mut Directory) -> FsResult<()> { if directory.closed { Err(FsError::invalid("directory handle is already closed")) } else { directory.closed = true; Ok(()) } }

/// Copies a file. See `uv.fs_copyfile()` in `runtime/doc/luvref.txt`.
pub fn copyfile(from: impl AsRef<Path>, to: impl AsRef<Path>, exclusive: bool) -> FsResult<u64> {
    if exclusive && to.as_ref().exists() { return Err(FsError { name: "EEXIST", message: "destination exists".into(), raw_os_error: None }); }
    fs::copy(from, to).map_err(FsError::from_io)
}
/// Creates a unique temporary directory from a trailing `XXXXXX` template. See `uv.fs_mkdtemp()` in `runtime/doc/luvref.txt`.
pub fn mkdtemp(template: impl AsRef<Path>) -> FsResult<PathBuf> { create_temp(template.as_ref(), |path| fs::create_dir(path).map(|()| ())) }
/// Creates a unique temporary file from a trailing `XXXXXX` template. See `uv.fs_mkstemp()` in `runtime/doc/luvref.txt`.
pub fn mkstemp(template: impl AsRef<Path>) -> FsResult<(FileHandle, PathBuf)> {
    let template = template.as_ref();
    let text = template.as_os_str().to_string_lossy();
    if !text.ends_with("XXXXXX") { return Err(FsError::invalid("temporary path template must end in XXXXXX")); }
    let prefix = &text[..text.len() - 6];
    for _ in 0..1024 {
        let candidate = temp_candidate(prefix);
        match OpenOptions::new().read(true).write(true).create_new(true).open(&candidate) {
            Ok(file) => return Ok((FileHandle::new(file), candidate)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(FsError { name: "EEXIST", message: "could not create a unique temporary file".into(), raw_os_error: None })
}

/// Updates followed path timestamps. See `uv.fs_utime()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn utime(path: impl AsRef<Path>, atime: FsTime, mtime: FsTime) -> FsResult<()> { set_times_at(path.as_ref(), atime, mtime, false) }
/// Updates symlink timestamps. See `uv.fs_lutime()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn lutime(path: impl AsRef<Path>, atime: FsTime, mtime: FsTime) -> FsResult<()> { set_times_at(path.as_ref(), atime, mtime, true) }
/// Updates open-file timestamps. See `uv.fs_futime()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn futime(handle: &FileHandle, atime: FsTime, mtime: FsTime) -> FsResult<()> {
    use rustix::fs::{Timestamps, futimens}; use rustix::time::Timespec;
    handle.with_file(|file| futimens(file, &Timestamps { last_access: Timespec { tv_sec: atime.sec, tv_nsec: i64::from(atime.nsec) }, last_modification: Timespec { tv_sec: mtime.sec, tv_nsec: i64::from(mtime.nsec) } }).map_err(|e| io::Error::from_raw_os_error(e.raw_os_error())))
}

/// Copies a range between open files. See `uv.fs_sendfile()` in `runtime/doc/luvref.txt`.
pub fn sendfile(out: &FileHandle, input: &FileHandle, offset: u64, size: usize) -> FsResult<usize> {
    let mut copied = 0usize; let mut buffer = vec![0u8; size.min(64 * 1024)];
    while copied < size {
        let wanted = buffer.len().min(size - copied);
        let count = input.with_file(|f| read_at(f, &mut buffer[..wanted], offset + copied as u64))?;
        if count == 0 { break; }
        let mut written = 0;
        while written < count {
            let count_written = out.with_file(|f| (&*f).write(&buffer[written..count]))?;
            if count_written == 0 { return Err(FsError::from_io(io::Error::from(io::ErrorKind::WriteZero))); }
            written += count_written;
        }
        copied += count;
    }
    Ok(copied)
}

/// Runs open on the pool. See `uv.fs_open()` in `runtime/doc/luvref.txt`.
pub fn open_async<P, C>(pool: &Pool, poster: P, path: PathBuf, flags: OpenFlags, mode: u32, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<FileHandle>) + Send + 'static { run_async(pool, poster, move || open(path, flags, mode), callback) }
/// Runs close on the pool. See `uv.fs_close()` in `runtime/doc/luvref.txt`.
pub fn close_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || close(&handle), callback) }
/// Runs read on the pool. See `uv.fs_read()` in `runtime/doc/luvref.txt`.
pub fn read_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, size: usize, offset: Option<u64>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Vec<u8>>) + Send + 'static { run_async(pool, poster, move || read(&handle, size, offset), callback) }
/// Runs write on the pool. See `uv.fs_write()` in `runtime/doc/luvref.txt`.
pub fn write_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, data: Vec<u8>, offset: Option<u64>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<usize>) + Send + 'static { run_async(pool, poster, move || write(&handle, &data, offset), callback) }
/// Runs a vectored read on the pool. See `uv.fs_read()` in `runtime/doc/luvref.txt`.
pub fn readv_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, sizes: Vec<usize>, offset: Option<u64>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Vec<Vec<u8>>>) + Send + 'static { run_async(pool, poster, move || readv(&handle, &sizes, offset), callback) }
/// Runs a vectored write on the pool. See `uv.fs_write()` in `runtime/doc/luvref.txt`.
pub fn writev_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, buffers: Vec<Vec<u8>>, offset: Option<u64>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<usize>) + Send + 'static { run_async(pool, poster, move || writev(&handle, &buffers, offset), callback) }
/// Runs mkdir on the pool. See `uv.fs_mkdir()` in `runtime/doc/luvref.txt`.
pub fn mkdir_async<P, C>(pool: &Pool, poster: P, path: PathBuf, mode: u32, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || mkdir(path, mode), callback) }
/// Runs rmdir on the pool. See `uv.fs_rmdir()` in `runtime/doc/luvref.txt`.
pub fn rmdir_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || rmdir(path), callback) }
/// Runs unlink on the pool. See `uv.fs_unlink()` in `runtime/doc/luvref.txt`.
pub fn unlink_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || unlink(path), callback) }
/// Runs rename on the pool. See `uv.fs_rename()` in `runtime/doc/luvref.txt`.
pub fn rename_async<P, C>(pool: &Pool, poster: P, from: PathBuf, to: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || rename(from, to), callback) }
/// Runs stat on the pool. See `uv.fs_stat()` in `runtime/doc/luvref.txt`.
pub fn stat_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Stat>) + Send + 'static { run_async(pool, poster, move || stat(path), callback) }
/// Runs lstat on the pool. See `uv.fs_lstat()` in `runtime/doc/luvref.txt`.
pub fn lstat_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Stat>) + Send + 'static { run_async(pool, poster, move || lstat(path), callback) }
/// Runs fstat on the pool. See `uv.fs_fstat()` in `runtime/doc/luvref.txt`.
pub fn fstat_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Stat>) + Send + 'static { run_async(pool, poster, move || fstat(&handle), callback) }
/// Runs link on the pool. See `uv.fs_link()` in `runtime/doc/luvref.txt`.
pub fn link_async<P, C>(pool: &Pool, poster: P, from: PathBuf, to: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || link(from, to), callback) }
/// Runs readlink on the pool. See `uv.fs_readlink()` in `runtime/doc/luvref.txt`.
pub fn readlink_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<PathBuf>) + Send + 'static { run_async(pool, poster, move || readlink(path), callback) }
/// Runs realpath on the pool. See `uv.fs_realpath()` in `runtime/doc/luvref.txt`.
pub fn realpath_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<PathBuf>) + Send + 'static { run_async(pool, poster, move || realpath(path), callback) }
/// Runs symlink on the pool. See `uv.fs_symlink()` in `runtime/doc/luvref.txt`.
pub fn symlink_async<P, C>(pool: &Pool, poster: P, target: PathBuf, path: PathBuf, directory: bool, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || symlink(target, path, directory), callback) }
/// Runs chmod on the pool. See `uv.fs_chmod()` in `runtime/doc/luvref.txt`.
pub fn chmod_async<P, C>(pool: &Pool, poster: P, path: PathBuf, mode: u32, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || chmod(path, mode), callback) }
/// Runs fchmod on the pool. See `uv.fs_fchmod()` in `runtime/doc/luvref.txt`.
pub fn fchmod_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, mode: u32, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || fchmod(&handle, mode), callback) }
/// Runs chown on the pool. See `uv.fs_chown()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn chown_async<P, C>(pool: &Pool, poster: P, path: PathBuf, uid: Option<u32>, gid: Option<u32>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || chown(path, uid, gid), callback) }
/// Runs fchown on the pool. See `uv.fs_fchown()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn fchown_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, uid: Option<u32>, gid: Option<u32>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || fchown(&handle, uid, gid), callback) }
/// Runs lchown on the pool. See `uv.fs_lchown()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn lchown_async<P, C>(pool: &Pool, poster: P, path: PathBuf, uid: Option<u32>, gid: Option<u32>, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || lchown(path, uid, gid), callback) }
/// Runs truncate on the pool. See `uv.fs_truncate()` in `runtime/doc/luvref.txt`.
pub fn truncate_async<P, C>(pool: &Pool, poster: P, path: PathBuf, length: u64, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || truncate(path, length), callback) }
/// Runs ftruncate on the pool. See `uv.fs_ftruncate()` in `runtime/doc/luvref.txt`.
pub fn ftruncate_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, length: u64, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || ftruncate(&handle, length), callback) }
/// Runs fsync on the pool. See `uv.fs_fsync()` in `runtime/doc/luvref.txt`.
pub fn fsync_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || fsync(&handle), callback) }
/// Runs fdatasync on the pool. See `uv.fs_fdatasync()` in `runtime/doc/luvref.txt`.
pub fn fdatasync_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || fdatasync(&handle), callback) }
/// Runs access on the pool. See `uv.fs_access()` in `runtime/doc/luvref.txt`.
pub fn access_async<P, C>(pool: &Pool, poster: P, path: PathBuf, read_ok: bool, write_ok: bool, execute_ok: bool, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<bool>) + Send + 'static { run_async(pool, poster, move || access(path, read_ok, write_ok, execute_ok), callback) }
/// Runs statfs on the pool. See `uv.fs_statfs()` in `runtime/doc/luvref.txt`.
pub fn statfs_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<StatFs>) + Send + 'static { run_async(pool, poster, move || statfs(path), callback) }
/// Runs scandir on the pool. See `uv.fs_scandir()` in `runtime/doc/luvref.txt`.
pub fn scandir_async<P, C>(pool: &Pool, poster: P, path: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Scandir>) + Send + 'static { run_async(pool, poster, move || scandir(path), callback) }
/// Runs opendir on the pool. See `uv.fs_opendir()` in `runtime/doc/luvref.txt`.
pub fn opendir_async<P, C>(pool: &Pool, poster: P, path: PathBuf, entries: usize, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<Directory>) + Send + 'static { run_async(pool, poster, move || opendir(path, entries), callback) }
/// Runs readdir on the pool while preserving stream ownership. See `uv.fs_readdir()` in `runtime/doc/luvref.txt`.
pub fn readdir_async<P, C>(pool: &Pool, poster: P, mut directory: Directory, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<(Directory, Vec<DirEntry>)>) + Send + 'static { run_async(pool, poster, move || { let entries = readdir(&mut directory)?; Ok((directory, entries)) }, callback) }
/// Runs closedir on the pool. See `uv.fs_closedir()` in `runtime/doc/luvref.txt`.
pub fn closedir_async<P, C>(pool: &Pool, poster: P, mut directory: Directory, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || closedir(&mut directory), callback) }
/// Runs copyfile on the pool. See `uv.fs_copyfile()` in `runtime/doc/luvref.txt`.
pub fn copyfile_async<P, C>(pool: &Pool, poster: P, from: PathBuf, to: PathBuf, exclusive: bool, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<u64>) + Send + 'static { run_async(pool, poster, move || copyfile(from, to, exclusive), callback) }
/// Runs mkdtemp on the pool. See `uv.fs_mkdtemp()` in `runtime/doc/luvref.txt`.
pub fn mkdtemp_async<P, C>(pool: &Pool, poster: P, template: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<PathBuf>) + Send + 'static { run_async(pool, poster, move || mkdtemp(template), callback) }
/// Runs mkstemp on the pool. See `uv.fs_mkstemp()` in `runtime/doc/luvref.txt`.
pub fn mkstemp_async<P, C>(pool: &Pool, poster: P, template: PathBuf, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<(FileHandle, PathBuf)>) + Send + 'static { run_async(pool, poster, move || mkstemp(template), callback) }
/// Runs sendfile on the pool. See `uv.fs_sendfile()` in `runtime/doc/luvref.txt`.
pub fn sendfile_async<P, C>(pool: &Pool, poster: P, out: FileHandle, input: FileHandle, offset: u64, size: usize, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<usize>) + Send + 'static { run_async(pool, poster, move || sendfile(&out, &input, offset, size), callback) }
/// Runs utime on the pool. See `uv.fs_utime()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn utime_async<P, C>(pool: &Pool, poster: P, path: PathBuf, atime: FsTime, mtime: FsTime, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || utime(path, atime, mtime), callback) }
/// Runs lutime on the pool. See `uv.fs_lutime()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn lutime_async<P, C>(pool: &Pool, poster: P, path: PathBuf, atime: FsTime, mtime: FsTime, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || lutime(path, atime, mtime), callback) }
/// Runs futime on the pool. See `uv.fs_futime()` in `runtime/doc/luvref.txt`.
#[cfg(unix)]
pub fn futime_async<P, C>(pool: &Pool, poster: P, handle: FileHandle, atime: FsTime, mtime: FsTime, callback: C) -> Result<(), PoolError> where P: LoopPoster, C: FnOnce(&mut UvLoop, FsResult<()>) + Send + 'static { run_async(pool, poster, move || futime(&handle, atime, mtime), callback) }

/// Runs any owned filesystem operation on the pool and posts its callback.
/// See `luv-file-system-operations` in `runtime/doc/luvref.txt`.
pub fn run_async<P, W, C, T>(pool: &Pool, poster: P, operation: W, callback: C) -> Result<(), PoolError>
where P: LoopPoster, W: FnOnce() -> FsResult<T> + Send + 'static, C: FnOnce(&mut UvLoop, FsResult<T>) + Send + 'static, T: Send + 'static {
    pool.submit(poster, operation, move |uv_loop, result| callback(uv_loop, result.unwrap_or_else(|error| Err(FsError::pool(error)))))
}

fn collect_entries(path: impl AsRef<Path>) -> FsResult<Vec<DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(FsError::from_io)? { let entry = entry.map_err(FsError::from_io)?; let kind = entry.file_type().map(classify_file_type).unwrap_or(DirEntryType::Unknown); entries.push(DirEntry { name: entry.file_name().to_string_lossy().into_owned(), kind }); }
    entries.sort_by(|left, right| left.name.cmp(&right.name)); Ok(entries)
}
#[cfg(unix)]
fn classify_file_type(kind: fs::FileType) -> DirEntryType {
    use std::os::unix::fs::FileTypeExt;
    if kind.is_file() { DirEntryType::File } else if kind.is_dir() { DirEntryType::Directory } else if kind.is_symlink() { DirEntryType::Symlink } else if kind.is_fifo() { DirEntryType::Fifo } else if kind.is_socket() { DirEntryType::Socket } else if kind.is_char_device() { DirEntryType::Character } else if kind.is_block_device() { DirEntryType::Block } else { DirEntryType::Unknown }
}
#[cfg(not(unix))]
fn classify_file_type(kind: fs::FileType) -> DirEntryType { if kind.is_file() { DirEntryType::File } else if kind.is_dir() { DirEntryType::Directory } else if kind.is_symlink() { DirEntryType::Symlink } else { DirEntryType::Unknown } }
fn create_temp<T>(template: &Path, create: impl Fn(&Path) -> io::Result<T>) -> FsResult<PathBuf> {
    let text = template.as_os_str().to_string_lossy(); if !text.ends_with("XXXXXX") { return Err(FsError::invalid("temporary path template must end in XXXXXX")); }
    let prefix = &text[..text.len() - 6];
    for _ in 0..1024 { let candidate = temp_candidate(prefix); match create(&candidate) { Ok(_) => return Ok(candidate), Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue, Err(error) => return Err(error.into()) } }
    Err(FsError { name: "EEXIST", message: "could not create a unique temporary path".into(), raw_os_error: None })
}
fn temp_candidate(prefix: &str) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_nanos();
    PathBuf::from(format!("{prefix}{:06x}", (stamp ^ u128::from(sequence) ^ u128::from(std::process::id())) & 0xff_ffff))
}

#[cfg(unix)]
fn read_at(file: &File, data: &mut [u8], offset: u64) -> io::Result<usize> { use std::os::unix::fs::FileExt; file.read_at(data, offset) }
#[cfg(windows)]
fn read_at(file: &File, data: &mut [u8], offset: u64) -> io::Result<usize> { use std::os::windows::fs::FileExt; file.seek_read(data, offset) }
#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, data: &mut [u8], offset: u64) -> io::Result<usize> { let mut file = file.try_clone()?; file.seek(SeekFrom::Start(offset))?; file.read(data) }
#[cfg(unix)]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<usize> { use std::os::unix::fs::FileExt; file.write_at(data, offset) }
#[cfg(windows)]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<usize> { use std::os::windows::fs::FileExt; file.seek_write(data, offset) }
#[cfg(not(any(unix, windows)))]
fn write_at(file: &File, data: &[u8], offset: u64) -> io::Result<usize> { let mut file = file.try_clone()?; file.seek(SeekFrom::Start(offset))?; file.write(data) }

#[cfg(unix)]
fn stat_from_metadata(m: &Metadata) -> Stat { use std::os::unix::fs::MetadataExt; Stat { dev: m.dev(), mode: m.mode(), nlink: m.nlink(), uid: m.uid(), gid: m.gid(), rdev: m.rdev(), ino: m.ino(), size: m.size(), blksize: m.blksize(), blocks: m.blocks(), flags: 0, r#gen: 0, atime: FsTime { sec: m.atime(), nsec: m.atime_nsec().try_into().unwrap_or(0) }, mtime: FsTime { sec: m.mtime(), nsec: m.mtime_nsec().try_into().unwrap_or(0) }, ctime: FsTime { sec: m.ctime(), nsec: m.ctime_nsec().try_into().unwrap_or(0) }, birthtime: system_time(m.created().ok()) } }
#[cfg(not(unix))]
fn stat_from_metadata(m: &Metadata) -> Stat { Stat { dev: 0, mode: 0, nlink: 0, uid: 0, gid: 0, rdev: 0, ino: 0, size: m.len(), blksize: 0, blocks: 0, flags: 0, r#gen: 0, atime: system_time(m.accessed().ok()), mtime: system_time(m.modified().ok()), ctime: FsTime { sec: 0, nsec: 0 }, birthtime: system_time(m.created().ok()) } }
fn system_time(value: Option<SystemTime>) -> FsTime { let duration = value.and_then(|v| v.duration_since(UNIX_EPOCH).ok()).unwrap_or(Duration::ZERO); FsTime { sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX), nsec: duration.subsec_nanos() } }

#[cfg(unix)]
fn set_times_at(path: &Path, atime: FsTime, mtime: FsTime, nofollow: bool) -> FsResult<()> { use rustix::fs::{AtFlags, CWD, Timestamps, utimensat}; use rustix::time::Timespec; let flags = if nofollow { AtFlags::SYMLINK_NOFOLLOW } else { AtFlags::empty() }; utimensat(CWD, path, &Timestamps { last_access: Timespec { tv_sec: atime.sec, tv_nsec: i64::from(atime.nsec) }, last_modification: Timespec { tv_sec: mtime.sec, tv_nsec: i64::from(mtime.nsec) } }, flags).map_err(|e| FsError::from_io(io::Error::from_raw_os_error(e.raw_os_error()))) }
#[cfg(unix)]
fn ownership_ids(uid: Option<u32>, gid: Option<u32>) -> FsResult<(Option<rustix::fs::Uid>, Option<rustix::fs::Gid>)> {
    if uid == Some(u32::MAX) || gid == Some(u32::MAX) { return Err(FsError::invalid("UID and GID must not be the all-ones sentinel")); }
    Ok((uid.map(rustix::fs::Uid::from_raw), gid.map(rustix::fs::Gid::from_raw)))
}

fn errno_name(raw: Option<i32>, kind: io::ErrorKind) -> &'static str {
    #[cfg(unix)]
    if let Some(raw) = raw {
        use rustix::io::Errno;
        let names = [
            (Errno::PERM, "EPERM"), (Errno::NOENT, "ENOENT"), (Errno::INTR, "EINTR"),
            (Errno::IO, "EIO"), (Errno::BADF, "EBADF"), (Errno::AGAIN, "EAGAIN"),
            (Errno::NOMEM, "ENOMEM"), (Errno::ACCESS, "EACCES"), (Errno::EXIST, "EEXIST"),
            (Errno::XDEV, "EXDEV"), (Errno::NOTDIR, "ENOTDIR"), (Errno::ISDIR, "EISDIR"),
            (Errno::INVAL, "EINVAL"), (Errno::NFILE, "ENFILE"), (Errno::MFILE, "EMFILE"),
            (Errno::FBIG, "EFBIG"), (Errno::NOSPC, "ENOSPC"), (Errno::ROFS, "EROFS"),
            (Errno::PIPE, "EPIPE"), (Errno::NAMETOOLONG, "ENAMETOOLONG"),
            (Errno::NOTEMPTY, "ENOTEMPTY"), (Errno::LOOP, "ELOOP"),
            (Errno::TIMEDOUT, "ETIMEDOUT"), (Errno::CANCELED, "ECANCELED"),
        ];
        if let Some((_, name)) = names.into_iter().find(|(errno, _)| errno.raw_os_error() == raw) { return name; }
    }
    match kind {
        io::ErrorKind::NotFound => "ENOENT", io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST", io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => "EINVAL",
        io::ErrorKind::TimedOut => "ETIMEDOUT", io::ErrorKind::Interrupted => "EINTR",
        io::ErrorKind::WouldBlock => "EAGAIN", io::ErrorKind::WriteZero => "EIO",
        io::ErrorKind::Unsupported => "ENOSYS", _ => "EIO",
    }
}
