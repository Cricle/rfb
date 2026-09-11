#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;

/// Boot as PID 1 with console attached (Linux).
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
#[cfg(target_os = "linux")]
pub fn init_pid1() -> std::io::Result<()> {
    init_pid1_with_console(true)
}

/// Boot as PID 1: mount proc/sys/dev, attach the console when requested, and
/// chdir into the workspace. No-op when not running as PID 1.
///
/// # Errors
///
/// Returns `Err` when the operation fails; the error type carries the cause.
#[cfg(target_os = "linux")]
pub fn init_pid1_with_console(attach_console: bool) -> std::io::Result<()> {
    if std::process::id() != 1 {
        return Ok(());
    }
    for path in ["/proc", "/sys", "/dev", "/run", "/tmp", "/workspace"] {
        fs::create_dir_all(path)?;
    }
    mount_if_needed("proc", "/proc", "proc")?;
    mount_if_needed("sysfs", "/sys", "sysfs")?;
    mount_if_needed("devtmpfs", "/dev", "devtmpfs")?;
    // The 8 MiB rootfs cannot hold investigation artifacts. Mount a bounded
    // in-memory tmpfs for the workspace so log/SQL dumps can be archived
    // without filling the image. Data is ephemeral (per-sandbox), matching the
    // sandbox lifecycle.
    mount_workspace_tmpfs()?;
    if attach_console {
        let tty = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/ttyS0")?;
        let fd = tty.as_raw_fd();
        for target in [0, 1, 2] {
            // SAFETY: `fd` is a live owned descriptor (tty is in scope) and
            // targets are the standard fds 0..=2; dup2 is async-signal-safe.
            if unsafe { libc::dup2(fd, target) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    std::env::set_current_dir("/workspace")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_if_needed(source: &str, target: &str, fstype: &str) -> std::io::Result<()> {
    let target_c = std::ffi::CString::new(target).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mount target contains NUL",
        )
    })?;
    if fs::metadata(target).is_ok()
        // SAFETY: every pointer is either null (allowed for mount(2)) or a
        // NUL-terminated CString that outlives the call.
        && unsafe {
            libc::mount(
                std::ptr::null(),
                target_c.as_ptr(),
                std::ptr::null(),
                libc::MS_RDONLY,
                std::ptr::null(),
            )
        } == 0
    {
        return Ok(());
    }
    let source = std::ffi::CString::new(source).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mount source contains NUL",
        )
    })?;
    let fstype = std::ffi::CString::new(fstype).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "mount type contains NUL")
    })?;
    let target = target_c;
    // SAFETY: all three pointers come from live CStrings; flags/data are
    // plain values, so the kernel only reads the provided buffers.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn mount_workspace_tmpfs() -> std::io::Result<()> {
    let target = std::ffi::CString::new("/workspace").expect("static path");
    let source = std::ffi::CString::new("tmpfs").expect("static source");
    let fstype = std::ffi::CString::new("tmpfs").expect("static fstype");
    let options = std::ffi::CString::new(format!(
        "size={}m,mode=0755",
        crate::resources::WORKSPACE_TMPFS_BYTES / (1024 * 1024)
    ))
    .expect("static options");
    // SAFETY: pointers reference live CStrings held until after the call;
    // the options pointer is a valid NUL-terminated C string cast to c_void.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            options.as_ptr() as *const libc::c_void,
        )
    };
    if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EBUSY) {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Non-Linux no-op variant of [`init_pid1`].
#[cfg(not(target_os = "linux"))]
pub fn init_pid1() -> std::io::Result<()> {
    Ok(())
}

/// Non-Linux no-op variant of [`init_pid1_with_console`].
#[cfg(not(target_os = "linux"))]
pub fn init_pid1_with_console(_attach_console: bool) -> std::io::Result<()> {
    Ok(())
}
