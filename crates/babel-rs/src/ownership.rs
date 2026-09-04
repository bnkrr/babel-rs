use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const LOCK_DIRECTORY: &str = "/run/babel-rs";

/// Process-lifetime ownership of one Linux route-protocol namespace.
///
/// The network-namespace inode is part of the key, so identical protocol
/// numbers remain usable in independent namespaces. `flock` is released by
/// the kernel on process exit; the harmless lock file may remain in /run.
pub struct ProtocolOwnership {
    _file: File,
}

impl ProtocolOwnership {
    pub fn acquire(protocol: u8) -> io::Result<Self> {
        fs::create_dir_all(LOCK_DIRECTORY)?;
        let namespace = fs::metadata("/proc/self/ns/net")?.ino();
        let path = Path::new(LOCK_DIRECTORY).join(format!("protocol-{namespace}-{protocol}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // SAFETY: flock only reads the valid file descriptor and retains no
        // pointer. The descriptor is held by Self until drop.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "Babel route protocol {protocol} is already owned in network namespace {namespace}"
                    ),
                ));
            }
            return Err(error);
        }
        Ok(Self { _file: file })
    }
}
