// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fs::File;
use std::io;

/// Returns the read and write data access of an opened descriptor.
pub fn file_data_access(file: &File) -> io::Result<(bool, bool)> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: F_GETFL only queries the live descriptor and takes no pointer.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "linux")]
    if flags & libc::O_PATH != 0 {
        return Ok((false, false));
    }
    let access = flags & libc::O_ACCMODE;
    Ok((access != libc::O_WRONLY, access != libc::O_RDONLY))
}

/// Clones all file extents into an independently writable destination.
pub fn reflink(source: &File, destination: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: Both arguments are live regular-file descriptors and FICLONE
        // does not retain either descriptor after the call.
        let result =
            unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file extent cloning is unsupported on this platform",
        ))
    }
}

/// Finds the next data extent at or after `offset`.
pub fn seek_data(file: &File, offset: u64) -> io::Result<Option<u64>> {
    seek_sparse(file, offset, libc::SEEK_DATA)
}

/// Finds the next hole at or after `offset`.
pub fn seek_hole(file: &File, offset: u64) -> io::Result<Option<u64>> {
    seek_sparse(file, offset, libc::SEEK_HOLE)
}

fn seek_sparse(file: &File, offset: u64, whence: i32) -> io::Result<Option<u64>> {
    use std::os::fd::AsRawFd;

    let offset = i64::try_from(offset)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds i64"))?;
    // SAFETY: The descriptor is live and `whence` is selected internally from
    // the platform's SEEK_DATA and SEEK_HOLE constants.
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result >= 0 {
        Ok(Some(result as u64))
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENXIO) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}
