use crate::error::{FetchError, Result};
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

pub fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(FetchError::io("Could not inspect destination", e)),
    }
}

pub fn open_regular(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if create {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    let file = options.open(path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            FetchError::Collision(path.to_path_buf())
        } else {
            FetchError::io("Could not open the download file", e)
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|e| FetchError::io("Could not inspect the download file", e))?;
    if !metadata.is_file() {
        return Err(FetchError::State("Expected a regular file.".into()));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(FetchError::State(
                "Linked download files are not supported.".into(),
            ));
        }
    }
    Ok(file)
}

pub fn lock(file: &File) -> Result<()> {
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => FetchError::Locked,
        std::fs::TryLockError::Error(e) => FetchError::io("Could not lock the saved download", e),
    })
}

pub fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| FetchError::State("Missing parent directory.".into()))?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| FetchError::io("Could not synchronize the destination folder", e))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn publish(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    let result = {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // No REPLACE_EXISTING and no COPY_ALLOWED: preserve collisions and stay on this volume.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    #[cfg(unix)]
    let result = {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| FetchError::InvalidInput("Invalid output path.".into()))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| FetchError::InvalidInput("Invalid output path.".into()))?;
        #[cfg(target_os = "linux")]
        let status = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let status =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let status = -1;
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    result.map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            FetchError::Collision(destination.to_path_buf())
        } else {
            FetchError::io(
                "Could not publish the completed file; the partial file has been kept",
                e,
            )
        }
    })?;
    sync_parent(destination)
}
