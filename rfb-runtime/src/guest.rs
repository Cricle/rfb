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
    // without filling the image. The size is derived from the guest's total
    // memory (40% of MemTotal, clamped to 64 MiB..1 GiB) — the same source as
    // `RuntimeLimits::max_workspace_bytes` — so the executor-side limit can
    // never exceed the kernel-enforced tmpfs cap. Data is ephemeral
    // (per-sandbox), matching the sandbox lifecycle.
    mount_workspace_tmpfs()?;
    // As pid 1, every daemonized process that double-forks out of an exec
    // reparents here, and nobody waits for it: exited orphans would pile up
    // as zombies until the process table exhausts. Start a low-frequency
    // reaper thread that reclaims them (see `OrphanReaper`).
    spawn_orphan_reaper();
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
    // No "already mounted" probe: mount(NULL, target, NULL, MS_RDONLY) can
    // never detect an existing mount (NULL fstype is ENODEV). Idempotency
    // comes solely from the EBUSY tolerance below, which is exactly what a
    // resumed (snapshot-restored) VM relies on — its mounts are inherited.
    let target_c = std::ffi::CString::new(target).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mount target contains NUL",
        )
    })?;
    let source = std::ffi::CString::new(source).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mount source contains NUL",
        )
    })?;
    let fstype = std::ffi::CString::new(fstype).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "mount type contains NUL")
    })?;
    // SAFETY: all three pointers come from live CStrings; flags/data are
    // plain values, so the kernel only reads the provided buffers.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
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
    // Same derived value the executor enforces (40% of MemTotal clamped to
    // 64 MiB..1 GiB; `/proc` is already mounted above), so a policy error can
    // never turn into raw ENOSPC from the mount. Both sides are MiB-aligned.
    let options = std::ffi::CString::new(format!(
        "size={}m,mode=0755",
        crate::resources::workspace_tmpfs_bytes() / (1024 * 1024)
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

/// How often the orphan reaper scans for new zombie children.
#[cfg(target_os = "linux")]
const ORPHAN_REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a zombie must stay unclaimed before the reaper reaps it.
///
/// This is a SAFETY bound, not a latency target: the workspace executor waits
/// its own children with targeted `waitpid(pid, WNOHANG)` (via
/// `Child::try_wait`), and a `waitpid(pid)` reaped by this thread can never be
/// reaped by the executor again — it would observe `ECHILD` and fail the turn
/// spuriously. A tracked child is normally claimed within one poll tick of
/// exiting, EXCEPT while the executor is still draining pipes that a
/// daemonized grandchild holds open — which can last a whole turn deadline
/// (`RuntimeLimits::max_runtime_seconds`, 1800 s). The grace must therefore
/// exceed the longest window in which any tracked child can legitimately sit
/// un-waited; only zombies every legitimate waiter has long stopped looking
/// for are reaped. Keep this constant above `max_runtime_seconds`.
#[cfg(target_os = "linux")]
const ORPHAN_REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(1800 + 60);

/// Reclaims zombie orphans that reparented to pid 1 without stealing the exit
/// status of children other threads (the workspace executor's `try_wait`, the
/// tokio process driver) are still tracking.
///
/// Why not the classic `loop { waitpid(-1, WNOHANG); sleep(...) }`: ANY
/// reaping — targeted or global — races the executor's targeted wait for the
/// same child, and a stolen status surfaces as a spurious `ECHILD` failure on
/// a turn that was still going to collect it. So this reaper only ever reaps
/// zombies that have been observed unclaimed for longer than
/// [`ORPHAN_REAP_GRACE`]: by then the executor's bounded turn (deadline-capped
/// terminate-and-reap) has long ended, and genuinely orphaned daemons have no
/// waiter at all.
///
/// Zombies are enumerated from `/proc` (state `Z`, parent pid 1) instead of
/// `waitid(P_ALL, WNOWAIT)` because a `WNOWAIT` peek leaves the child in
/// place: the next peek would return the same young zombie forever, with no
/// way to advance past it to older ones.
#[cfg(target_os = "linux")]
struct OrphanReaper {
    /// Parent pid whose zombie children are reaped (1 in production).
    parent: i32,
    /// First-seen time per zombie pid, so grace is measured from the first
    /// observation rather than trusting /proc ordering.
    seen: std::collections::HashMap<i32, std::time::Instant>,
}

#[cfg(target_os = "linux")]
impl OrphanReaper {
    fn new(parent: i32) -> Self {
        Self {
            parent,
            seen: std::collections::HashMap::new(),
        }
    }

    /// One scan cycle: reap zombies older than `grace`, return their pids.
    fn scan(&mut self, grace: std::time::Duration) -> Vec<i32> {
        let zombies = zombie_children_of(self.parent);
        let mut reaped = Vec::new();
        for pid in zombies.iter().copied() {
            let first = match self.seen.get(&pid) {
                Some(first) => *first,
                None => {
                    self.seen.insert(pid, std::time::Instant::now());
                    continue;
                }
            };
            if first.elapsed() >= grace {
                // SAFETY: waitpid on a pid that /proc just reported as our own
                // zombie child; the status pointer is NULL (status discarded),
                // so no memory is shared with the kernel for this call.
                let rc = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                self.seen.remove(&pid);
                if rc > 0 {
                    reaped.push(pid);
                }
                // rc == 0 / -1: the pid is no longer a zombie (reaped by a
                // racing waiter, or the pid was reused) — nothing to report.
            }
        }
        // A pid we were aging that no longer shows up as a zombie was reaped
        // by its legitimate owner: forget it so the map cannot grow with
        // stale entries (or mis-age a reused pid).
        self.seen.retain(|pid, _| zombies.contains(pid));
        reaped
    }
}

/// Spawn the daemon reaper thread (pid 1 only; fire-and-forget).
#[cfg(target_os = "linux")]
fn spawn_orphan_reaper() {
    let spawned = std::thread::Builder::new()
        .name("rfb-pid1-orphan-reaper".into())
        .spawn(|| {
            let mut reaper = OrphanReaper::new(1);
            loop {
                std::thread::sleep(ORPHAN_REAP_INTERVAL);
                let reaped = reaper.scan(ORPHAN_REAP_GRACE);
                for pid in reaped {
                    eprintln!("rfb-guest: reaped orphaned zombie pid {pid}");
                }
            }
        });
    // The reaper is hygiene, not correctness of boot: a failed spawn (thread
    // exhaustion) must not abort pid-1 init. Zombies then accumulate until the
    // VM dies — the historical behavior, not a new failure mode.
    if let Err(error) = spawned {
        eprintln!("rfb-guest: orphan reaper thread failed to spawn: {error}");
    }
}

/// Collect the zombie children of `parent` by scanning /proc. Parsing a stat
/// line starts after the LAST `)`: the comm field may contain spaces and
/// parentheses, and the fields that follow are state then ppid (proc(5)).
#[cfg(target_os = "linux")]
fn zombie_children_of(parent: i32) -> Vec<i32> {
    let mut zombies = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return zombies;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[after_comm + 1..].split_ascii_whitespace();
        let Some(state) = fields.next() else {
            continue;
        };
        if !state.starts_with('Z') {
            continue;
        }
        if fields.next().and_then(|p| p.parse::<i32>().ok()) == Some(parent) {
            zombies.push(pid);
        }
    }
    zombies
}

/// Non-Linux no-op variant of [`init_pid1_with_console`].
#[cfg(not(target_os = "linux"))]
pub fn init_pid1_with_console(_attach_console: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod orphan_reaper_tests {
    use super::*;
    use std::time::Duration;

    /// The reaper scans every zombie of the whole test process, so parallel
    /// test threads would reap each other's fixtures. Serialize the tests
    /// instead (cargo runs tests in parallel by default).
    static REAPER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        // A panicking predecessor poisons the lock; the tests are equivalent,
        // so carry on regardless.
        REAPER_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Spawn a child that exits immediately and drop the handle without
    /// waiting: std's `Child` does not reap on drop, so it becomes a zombie
    /// of this test process. The reaper's parent is a parameter (1 in
    /// production) precisely so tests can point it at their own process.
    fn spawn_unreaped_child() -> i32 {
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn /bin/true");
        let pid = child.id() as i32;
        drop(child);
        pid
    }

    /// Block until the child shows up as a zombie (spawning and exiting race
    /// the first scan otherwise).
    fn wait_until_zombie(pid: i32) {
        let parent = std::process::id() as i32;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if zombie_children_of(parent).contains(&pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("pid {pid} never became a zombie");
    }

    /// Clean up a zombie the test decided not to let the reaper reap.
    fn wait_child(pid: i32) {
        // SAFETY: direct waitpid on the test's own child pid.
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }

    #[test]
    fn young_zombies_are_never_stolen_from_their_waiter() {
        let _guard = lock();
        let pid = spawn_unreaped_child();
        wait_until_zombie(pid);
        let mut reaper = OrphanReaper::new(std::process::id() as i32);
        // Repeated scans with a grace the zombie has not reached must age it
        // without reaping: its legitimate waiter (executor `try_wait`) may
        // still be coming.
        assert!(reaper.scan(Duration::from_secs(3600)).is_empty());
        assert!(reaper.scan(Duration::from_secs(3600)).is_empty());
        wait_child(pid);
    }

    #[test]
    fn aged_orphans_are_reaped_exactly_once() {
        let _guard = lock();
        let pid = spawn_unreaped_child();
        wait_until_zombie(pid);
        let mut reaper = OrphanReaper::new(std::process::id() as i32);
        // First sight only starts the aging (never reaps on the same scan).
        assert!(reaper.scan(Duration::ZERO).is_empty());
        assert_eq!(
            reaper.scan(Duration::ZERO),
            vec![pid],
            "an orphan past its grace must be reaped and reported"
        );
        // Reaped for good: no double report, no stale bookkeeping.
        assert!(reaper.scan(Duration::ZERO).is_empty());
    }

    #[test]
    fn zombies_reaped_by_their_owner_are_forgotten() {
        let _guard = lock();
        let pid = spawn_unreaped_child();
        wait_until_zombie(pid);
        let mut reaper = OrphanReaper::new(std::process::id() as i32);
        assert!(reaper.scan(Duration::from_secs(3600)).is_empty());
        // The legitimate owner reaps it out from under the reaper.
        wait_child(pid);
        // The reaper must drop the stale entry instead of aging a pid that no
        // longer is a zombie (a later scan reports nothing and the map
        // shrinks back).
        assert!(reaper.scan(Duration::ZERO).is_empty());
        assert!(reaper.seen.is_empty());
    }
}
