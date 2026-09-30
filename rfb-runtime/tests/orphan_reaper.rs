//! PID-1 orphan reaper white-box tests (integration tests; moved out of
//! src/ per the check-tests-folder boundary). The reaper scans every
//! zombie of the whole test process, so the module's lock serializes
//! the tests against each other.
#![cfg(all(feature = "guest", target_os = "linux"))]

mod orphan_reaper_tests {
    use rfb_runtime::guest::{zombie_children_of, OrphanReaper};

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
