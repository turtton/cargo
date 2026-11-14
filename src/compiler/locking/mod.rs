//! Lock order: package GC SH -> writer gate EX -> unit EX/SH -> output-link lock.
//! A gate holder never waits for unit EX. Reader conflicts are bounded; a
//! live writer is not subject to that deadline. All OS waits are outside the
//! manager's map mutexes, and every writer owns its file description.

use crate::compiler::fingerprint::PreparedFreshnessProbe;
use crate::compiler::job_queue::{JobState, Work};
use crate::util::data_structures::HashMap;
use crate::util::{FileLock, Filesystem, flock};
use crate::{
    CargoResult,
    compiler::{BuildRunner, Unit},
};
use anyhow::{Context as _, bail};
use rand::RngExt as _;
use std::fmt::{Display, Formatter};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

mod test_hook;
pub(in crate::compiler) use test_hook::TestHook;

pub(super) const GC_LOCK_FILE: &str = ".cargo-gc-lock";
pub(super) const WRITE_LOCK_FILE: &str = ".cargo-unit-write-lock";
const TIMEOUT_ENV: &str = "CARGO_FORK_UNIT_CONFLICT_TIMEOUT_SECS";
type Slot = Arc<Mutex<Option<FileLock>>>;

#[derive(Default)]
struct UnitEntry {
    writer_owned: AtomicBool,
}

/// Retains readers and GC protection until all scoped workers have finished.
pub struct LockManager {
    units: Mutex<HashMap<PathBuf, Arc<UnitEntry>>>,
    slots: Mutex<Vec<Slot>>,
    gc_locks: Mutex<HashMap<PathBuf, FileLock>>,
    cancelled: AtomicBool,
    wait_mutex: Mutex<()>,
    wake: Condvar,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            units: Mutex::new(HashMap::default()),
            slots: Mutex::new(Vec::new()),
            gc_locks: Mutex::new(HashMap::default()),
            cancelled: AtomicBool::new(false),
            wait_mutex: Mutex::new(()),
            wake: Condvar::new(),
        }
    }

    /// Acquire every package's GC protection before the first unit lock.
    /// GC only tries EX, so a builder's blocking SH can wait only for GC's
    /// short deletion interval, never for a GC waiting on a builder unit.
    pub fn prepare_package_gc_locks(&self, build_runner: &BuildRunner<'_, '_>) -> CargoResult<()> {
        if !build_runner.bcx.gctx.cli_unstable().fine_grain_locking
            || !build_runner.bcx.gctx.cli_unstable().build_dir_new_layout
            || crate::workspace::features::is_fork_defaults_opt_out()
        {
            return Ok(());
        }
        debug_assert!(self.slots.lock().unwrap().is_empty());
        debug_assert!(self.units.lock().unwrap().is_empty());
        let mut packages: Vec<_> = build_runner
            .bcx
            .unit_graph
            .keys()
            .map(|unit| {
                let path = build_runner.files().build_unit_lock(unit);
                (path.parent().unwrap().parent().unwrap().to_path_buf(), unit)
            })
            .collect();
        packages.sort_by(|a, b| a.0.cmp(&b.0));
        packages.dedup_by(|a, b| a.0 == b.0);
        for (path, unit) in packages {
            // One descriptor per physical package directory (deduplicated
            // across its compile/run-build-script units). No map mutex spans IO.
            let lock = Filesystem::new(path.clone()).open_ro_shared_create(
                GC_LOCK_FILE, build_runner.bcx.gctx, "package build directory",
            ).with_context(|| format!("failed to acquire package GC protection at {}; one file descriptor is required per package build directory", path.display()))?;
            self.gc_locks.lock().unwrap().insert(path.clone(), lock);
            TestHook::new(build_runner, unit, &path.join(GC_LOCK_FILE))
                .event("PackageGcAcquired")?;
        }
        Ok(())
    }

    pub fn lock_shared(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
    ) -> CargoResult<LockKey> {
        let path = build_runner.files().build_unit_lock(unit);
        let hook = TestHook::new(build_runner, unit, &path);
        let timeout = conflict_timeout(build_runner.bcx.gctx.get_env_os(TIMEOUT_ENV))?;
        if build_runner.bcx.gctx.cli_unstable().build_dir_new_layout
            && !crate::workspace::features::is_fork_defaults_opt_out()
        {
            let pkg_dir = path.parent().unwrap().parent().unwrap().to_path_buf();
            let held = self.gc_locks.lock().unwrap().contains_key(&pkg_dir);
            if !held {
                return Err(crate::util::internal(format!(
                    "package GC protection was not prepared before unit locking: {}",
                    pkg_dir.display()
                )));
            }
        }
        let description = format!(
            "package `{}` target `{}` unit `{}`",
            unit.pkg.name(),
            unit.target.name(),
            build_runner.files().unit_hash(unit)
        );
        let fs = Filesystem::new(path.clone());
        let mut announced = false;
        let file = loop {
            if let Some(file) = fs.try_open_ro_shared_create(&path)? {
                break file;
            }
            if !announced {
                build_runner.bcx.gctx.shell().status(
                    "Blocking",
                    format!("waiting for writer of {description} at {}", path.display()),
                )?;
                announced = true;
            }
            if hook.event("WaitingWriter")?.is_none() {
                self.pause(backoff(0))?;
            }
        };
        hook.event("PlanningShared")?;
        let slot = Arc::new(Mutex::new(Some(file)));
        self.slots.lock().unwrap().push(Arc::clone(&slot));
        let unit = Arc::clone(self.units.lock().unwrap().entry(path.clone()).or_default());
        Ok(LockKey {
            path,
            description,
            timeout,
            slot,
            unit,
            hook,
        })
    }

    pub fn unlock(&self, key: &LockKey) -> CargoResult<()> {
        let file = key.slot.lock().unwrap();
        flock::unlock(
            file.as_ref()
                .context("unit lock is owned by a running job")?
                .file(),
        )?;
        Ok(())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        let _lock = self.wait_mutex.lock().unwrap();
        self.cancelled.store(true, Ordering::Release);
        self.wake.notify_all();
    }
    fn check_cancelled(&self) -> CargoResult<()> {
        if self.is_cancelled() {
            bail!("unit lock wait cancelled after another build job failed");
        }
        Ok(())
    }
    fn pause(&self, duration: Duration) -> CargoResult<Duration> {
        let lock = self.wait_mutex.lock().unwrap();
        self.check_cancelled()?;
        let start = Instant::now();
        let (_lock, _) = self.wake.wait_timeout(lock, duration).unwrap();
        self.check_cancelled()?;
        Ok(start.elapsed())
    }
    fn wait(&self, key: &LockKey, kind: WaitKind, duration: Duration) -> CargoResult<Duration> {
        let phase = match kind {
            WaitKind::WaitingWriter => "WaitingWriter",
            WaitKind::ConflictingReaders => "ConflictingReaders",
        };
        if let Some(elapsed) = key.hook.event(phase)? {
            self.check_cancelled()?;
            Ok(elapsed)
        } else {
            self.pause(duration)
        }
    }

    pub fn release_gc_locks(&self) {
        self.gc_locks.lock().unwrap().clear();
    }
    /// Only after thread::scope has joined every writer (including its child).
    pub fn release_all(&self) {
        for slot in self.slots.lock().unwrap().iter() {
            slot.lock().unwrap().take();
        }
        self.release_gc_locks();
    }

    pub(super) fn run_locked(
        &self,
        key: &LockKey,
        probe: PreparedFreshnessProbe,
        dirty: Work,
        done: Work,
        fresh: Work,
        state: &JobState<'_, '_>,
    ) -> CargoResult<()> {
        key.hook.event("ReadyToRun")?;
        let (mut guard, reused) = self.acquire(key, &|| probe.is_fresh(), &|message| {
            state.unit_lock_status(message.to_owned())
        })?;
        if reused {
            key.hook.event("SharedValidated")?;
            guard.release_gate();
            key.hook.event("GateReleased")?;
            key.hook.event("FreshReuse")?;
            return fresh.call(state);
        }
        key.hook.event("WriterAcquired")?;
        if let Err(error) = probe.invalidate() {
            self.cancel();
            return Err(error);
        }
        // A command failure has already waited for its child to exit. Keep
        // normal --keep-going semantics; only protocol failures are fatal.
        dirty.call(state)?;
        key.hook.event("WorkFinished")?;
        let result = (|| {
            guard.downgrade(self)?;
            if !probe.verify_written()? {
                bail!(
                    "unit changed while finalizing {} at {}; refusing to report success",
                    key.description,
                    key.path.display()
                );
            }
            key.hook.event("SharedValidated")?;
            guard.release_gate();
            key.hook.event("GateReleased")?;
            Ok(())
        })();
        if result.is_err() {
            self.cancel();
            key.hook.event("FatalConflict")?;
        }
        result?;
        done.call(state)
    }

    fn acquire(
        &self,
        key: &LockKey,
        fresh: &dyn Fn() -> CargoResult<bool>,
        status: &dyn Fn(&str) -> CargoResult<()>,
    ) -> CargoResult<(OwnedUnit, bool)> {
        let result = self.acquire_inner(key, fresh, status);
        if result.is_err() {
            self.cancel();
            key.hook.event("FatalConflict")?;
        }
        result
    }

    fn acquire_inner(
        &self,
        key: &LockKey,
        fresh: &dyn Fn() -> CargoResult<bool>,
        status: &dyn Fn(&str) -> CargoResult<()>,
    ) -> CargoResult<(OwnedUnit, bool)> {
        let mut budget = ConflictBudget::default();
        let mut announced = false;
        let mut attempt = 0;
        loop {
            self.check_cancelled()?;
            let Some(mut guard) = OwnedUnit::claim(key)? else {
                announce(&mut announced, key, status)?;
                budget.record(
                    WaitKind::WaitingWriter,
                    self.wait(key, WaitKind::WaitingWriter, backoff(attempt))?,
                );
                continue;
            };
            if !guard.try_gate()? {
                drop(guard);
                announce(&mut announced, key, status)?;
                // WaitingWriter: deliberately do not charge the conflict budget.
                budget.record(
                    WaitKind::WaitingWriter,
                    self.wait(key, WaitKind::WaitingWriter, backoff(attempt))?,
                );
                continue;
            }
            if try_lock(flock::try_lock_exclusive(guard.file()), key)? {
                if fresh()? {
                    guard.downgrade(self)?;
                    if !fresh()? {
                        bail!("unit freshness changed while downgrading {}", key);
                    }
                    return Ok((guard, true));
                }
                return Ok((guard, false));
            }

            // A compliant writer would hold the gate. Never use blocking SH:
            // flock conversion is not assumed atomic, even on Linux.
            let mut shared = false;
            for _ in 0..8 {
                if try_lock(flock::try_lock_shared(guard.file()), key)? {
                    shared = true;
                    break;
                }
                self.pause(Duration::from_millis(5))?;
            }
            if !shared {
                bail!(
                    "cannot acquire shared unit lock while owning writer gate for {} at {}; an older Cargo or non-compliant writer may be using this directory",
                    key.description,
                    key.path.display()
                );
            }
            guard.shared = true;
            if fresh()? {
                return Ok((guard, true));
            }
            guard.unlock()?;
            if try_lock(flock::try_lock_exclusive(guard.file()), key)? {
                return Ok((guard, false));
            }
            drop(guard); // No EX wait or backoff with the gate held.
            announce(&mut announced, key, status)?;
            // The last iteration still tried freshness and EX before expiry.
            if budget.spent >= key.timeout {
                bail!(
                    "timed out waiting for conflicting readers of {} at {} ({} seconds of reader contention). Another worktree or build configuration may be using these outputs; retrying after it exits may resolve the conflict",
                    key.description,
                    key.path.display(),
                    key.timeout.as_secs()
                );
            }
            budget.record(
                WaitKind::ConflictingReaders,
                self.wait(key, WaitKind::ConflictingReaders, backoff(attempt))?,
            );
            attempt = attempt.saturating_add(1);
        }
    }
}

fn announce(
    announced: &mut bool,
    key: &LockKey,
    status: &dyn Fn(&str) -> CargoResult<()>,
) -> CargoResult<()> {
    if !*announced {
        status(&format!(
            "waiting for {} at {}",
            key.description,
            key.path.display()
        ))?;
        *announced = true;
    }
    Ok(())
}

fn conflict_timeout(value: Option<&std::ffi::OsStr>) -> CargoResult<Duration> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(600));
    };
    let error = || anyhow::anyhow!("{TIMEOUT_ENV} must be a positive integer number of seconds");
    let value = value.to_str().ok_or_else(error)?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error());
    }
    let seconds: u64 = value.parse().map_err(|_| error())?;
    if seconds == 0 {
        return Err(error());
    }
    Ok(Duration::from_secs(seconds))
}

fn try_lock(result: Result<(), TryLockError>, key: &LockKey) -> CargoResult<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("failed to lock {}", key.path.display()))
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    let cap = (5u64 << attempt.min(6)).min(250);
    Duration::from_millis(rand::rng().random_range(cap / 2..=cap))
}

#[derive(Clone, Copy)]
enum WaitKind {
    WaitingWriter,
    ConflictingReaders,
}
#[derive(Default)]
struct ConflictBudget {
    spent: Duration,
}
impl ConflictBudget {
    fn record(&mut self, kind: WaitKind, elapsed: Duration) {
        if matches!(kind, WaitKind::ConflictingReaders) {
            self.spent = self.spent.saturating_add(elapsed);
        }
    }
}

/// A unique file description plus per-path ownership. Clones share a slot,
/// never a writer guard. Distinct local jobs use distinct descriptions, so
/// neither flock reentrancy nor an alias can bypass a retained local reader.
#[derive(Clone)]
pub struct LockKey {
    path: PathBuf,
    description: String,
    timeout: Duration,
    slot: Slot,
    unit: Arc<UnitEntry>,
    hook: TestHook,
}
impl LockKey {
    pub(super) fn event(&self, phase: &str) -> CargoResult<()> {
        self.hook.event(phase)?;
        Ok(())
    }
}
impl Display for LockKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

struct OwnedUnit {
    key: LockKey,
    file: Option<FileLock>,
    gate: Option<File>,
    shared: bool,
}
impl OwnedUnit {
    fn claim(key: &LockKey) -> CargoResult<Option<Self>> {
        if key
            .unit
            .writer_owned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(None);
        }
        let file = key.slot.lock().unwrap().take();
        if file.is_none() {
            key.unit.writer_owned.store(false, Ordering::Release);
            bail!("unit lock file missing for {}", key);
        }
        Ok(Some(Self {
            key: key.clone(),
            file,
            gate: None,
            shared: false,
        }))
    }
    fn file(&self) -> &File {
        self.file.as_ref().unwrap().file()
    }
    fn try_gate(&mut self) -> CargoResult<bool> {
        let path = self.key.path.parent().unwrap().join(WRITE_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open writer gate {}", path.display()))?;
        if try_lock(flock::try_lock_exclusive(&file), &self.key)? {
            self.gate = Some(file);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn unlock(&mut self) -> CargoResult<()> {
        flock::unlock(self.file())?;
        self.shared = false;
        Ok(())
    }
    fn downgrade(&mut self, manager: &LockManager) -> CargoResult<()> {
        debug_assert!(self.gate.is_some());
        // Make the conversion gap explicit; the writer gate, not flock's
        // conversion atomicity, protects this transition.
        self.unlock()?;
        self.key.hook.event("DowngradeGap")?;
        for _ in 0..8 {
            if try_lock(flock::try_lock_shared(self.file()), &self.key)? {
                self.shared = true;
                return Ok(());
            }
            manager.pause(Duration::from_millis(5))?;
        }
        bail!(
            "cannot regain shared unit lock while owning writer gate at {}; an older Cargo or non-compliant writer may be using this directory",
            self.key.path.display()
        )
    }
    fn release_gate(&mut self) {
        self.gate.take();
    }
}
impl Drop for OwnedUnit {
    fn drop(&mut self) {
        if !self.shared {
            let _ = flock::unlock(self.file());
        }
        *self.key.slot.lock().unwrap() = self.file.take();
        self.gate.take();
        self.key.unit.writer_owned.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(dir: &std::path::Path, unit: Arc<UnitEntry>) -> LockKey {
        let path = dir.join(".lock");
        let file = Filesystem::new(dir.to_path_buf())
            .try_open_ro_shared_create(&path)
            .unwrap()
            .unwrap();
        flock::unlock(file.file()).unwrap();
        LockKey {
            path,
            description: "test unit".into(),
            timeout: Duration::from_secs(1),
            slot: Arc::new(Mutex::new(Some(file))),
            unit,
            hook: TestHook::default(),
        }
    }

    #[test]
    fn local_writer_ownership_is_not_reentrant() {
        let dir = tempfile::tempdir().unwrap();
        let unit = Arc::new(UnitEntry::default());
        let a = key(dir.path(), Arc::clone(&unit));
        let b = key(dir.path(), unit);
        let owner = OwnedUnit::claim(&a).unwrap().unwrap();
        assert!(OwnedUnit::claim(&a.clone()).unwrap().is_none());
        assert!(OwnedUnit::claim(&b).unwrap().is_none());
        drop(owner);
        assert!(OwnedUnit::claim(&b).unwrap().is_some());
    }

    #[test]
    fn gate_protects_downgrade_and_reader_survives_gate_release() {
        let dir = tempfile::tempdir().unwrap();
        let key = key(dir.path(), Arc::new(UnitEntry::default()));
        let manager = LockManager::new();
        let (mut owner, reused) = manager.acquire(&key, &|| Ok(false), &|_| Ok(())).unwrap();
        assert!(!reused);
        let gate = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join(WRITE_LOCK_FILE))
            .unwrap();
        assert!(matches!(
            flock::try_lock_exclusive(&gate),
            Err(TryLockError::WouldBlock)
        ));
        owner.downgrade(&manager).unwrap();
        assert!(matches!(
            flock::try_lock_exclusive(&gate),
            Err(TryLockError::WouldBlock)
        ));
        owner.release_gate();
        flock::try_lock_exclusive(&gate).unwrap();
        let observer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&key.path)
            .unwrap();
        assert!(matches!(
            flock::try_lock_exclusive(&observer),
            Err(TryLockError::WouldBlock)
        ));
        drop(owner);
        assert!(matches!(
            flock::try_lock_exclusive(&observer),
            Err(TryLockError::WouldBlock)
        ));
        key.slot.lock().unwrap().take();
        flock::try_lock_exclusive(&observer).unwrap();
    }

    #[test]
    fn cancellation_does_not_drop_an_active_writers_protection() {
        let dir = tempfile::tempdir().unwrap();
        let key = key(dir.path(), Arc::new(UnitEntry::default()));
        let manager = LockManager::new();
        let (owner, _) = manager.acquire(&key, &|| Ok(false), &|_| Ok(())).unwrap();
        manager.cancel();
        let observer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&key.path)
            .unwrap();
        assert!(matches!(
            flock::try_lock_exclusive(&observer),
            Err(TryLockError::WouldBlock)
        ));
        assert!(manager.acquire(&key, &|| Ok(false), &|_| Ok(())).is_err());
        drop(owner);
        flock::try_lock_exclusive(&observer).unwrap();
    }

    #[test]
    fn timeout_configuration_is_strict_and_defaults_to_ten_minutes() {
        assert_eq!(conflict_timeout(None).unwrap(), Duration::from_secs(600));
        assert_eq!(
            conflict_timeout(Some("1".as_ref())).unwrap(),
            Duration::from_secs(1)
        );
        for bad in ["", "0", "-1", "+1", "1.5", " 1", "18446744073709551616"] {
            assert!(conflict_timeout(Some(bad.as_ref())).is_err(), "{bad:?}");
        }
    }
    #[test]
    fn writer_wait_does_not_consume_reader_budget() {
        let mut budget = ConflictBudget::default();
        budget.record(WaitKind::ConflictingReaders, Duration::from_secs(9));
        budget.record(WaitKind::WaitingWriter, Duration::from_secs(1000));
        assert_eq!(budget.spent, Duration::from_secs(9));
        budget.record(WaitKind::ConflictingReaders, Duration::from_secs(2));
        assert_eq!(budget.spent, Duration::from_secs(11));
    }
}
