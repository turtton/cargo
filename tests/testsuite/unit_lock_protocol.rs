//! Cross-process protocol tests use event acknowledgements as barriers and
//! controller-supplied elapsed time. No elapsed wall time decides correctness.
#![cfg(all(unix, debug_assertions))]

use crate::prelude::*;
use cargo_test_support::{Project, git, paths, project};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct Event {
    data: Value,
    socket: Option<UnixStream>,
}

impl Event {
    fn text(&self, key: &str) -> &str {
        self.data[key].as_str().unwrap()
    }

    fn lock(&self) -> PathBuf {
        self.text("lock").into()
    }

    fn resume(mut self, elapsed_millis: u64) {
        writeln!(self.socket.take().unwrap(), "{elapsed_millis}").unwrap();
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        if let Some(socket) = &mut self.socket {
            let _ = writeln!(socket, "0");
        }
    }
}

#[derive(Clone, Debug)]
struct Exit {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

enum Notice {
    Event(Event),
    Exit(u32, Exit),
}

struct Controller {
    dir: tempfile::TempDir,
    tx: Sender<Notice>,
    rx: Receiver<Notice>,
    listener: Option<JoinHandle<()>>,
    exits: BTreeMap<u32, Exit>,
    history: Vec<Value>,
    next: usize,
}

impl Controller {
    fn new() -> Self {
        // Project paths can exceed sockaddr_un's limit. TMPDIR is deliberately
        // short in the test environment; tempfile supplies collision isolation.
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("events")).unwrap();
        let (tx, rx) = mpsc::channel();
        let sender = tx.clone();
        let listener = thread::spawn(move || {
            for socket in listener.incoming() {
                let socket = socket.unwrap();
                let mut line = String::new();
                BufReader::new(&socket).read_line(&mut line).unwrap();
                if line == "STOP\n" {
                    break;
                }
                let data = serde_json::from_str(&line).unwrap();
                if sender
                    .send(Notice::Event(Event {
                        data,
                        socket: Some(socket),
                    }))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            dir,
            tx,
            rx,
            listener: Some(listener),
            exits: BTreeMap::new(),
            history: Vec::new(),
            next: 0,
        }
    }

    fn spawn(&mut self, mut command: Command) -> Build {
        let stdout = self.dir.path().join(format!("{}.stdout", self.next));
        let stderr = self.dir.path().join(format!("{}.stderr", self.next));
        self.next += 1;
        command
            .env(
                "__CARGO_TEST_UNIT_LOCK_SOCKET",
                self.dir.path().join("events"),
            )
            .env("CARGO_FORK_UNIT_CONFLICT_TIMEOUT_SECS", "1")
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let sender = self.tx.clone();
        let (done, cleanup) = mpsc::channel();
        // Cleanup only: protocol assertions await explicit events/child exits.
        // This watchdog prevents an unexpected regression from leaking children
        // if the test runner terminates a blocked test.
        let watchdog = thread::spawn(move || {
            if cleanup.recv_timeout(Duration::from_secs(180)).is_err() {
                kill_group(pid);
            }
        });
        let worker = thread::spawn(move || {
            let status = child.wait().unwrap();
            let _ = done.send(());
            let exit = Exit {
                status,
                stdout: fs::read_to_string(stdout).unwrap(),
                stderr: fs::read_to_string(stderr).unwrap(),
            };
            let _ = sender.send(Notice::Exit(pid, exit));
        });
        Build {
            pid,
            worker: Some(worker),
            watchdog: Some(watchdog),
        }
    }

    fn until(&mut self, pid: u32, phase: &str, package: &str, mode: &str) -> Event {
        loop {
            assert!(
                !self.exits.contains_key(&pid),
                "process {pid} exited before {phase}: {:?}",
                self.exits.get(&pid)
            );
            match self.rx.recv().unwrap() {
                Notice::Event(event) => {
                    self.history.push(event.data.clone());
                    if event.data["pid"] == pid
                        && event.text("phase") == phase
                        && (package.is_empty() || event.text("package") == package)
                        && (mode.is_empty() || event.text("mode") == mode)
                    {
                        return event;
                    }
                    event.resume(0);
                }
                Notice::Exit(pid, exit) => {
                    self.exits.insert(pid, exit);
                }
            }
        }
    }

    fn wait(&mut self, pid: u32) -> Exit {
        loop {
            if let Some(exit) = self.exits.remove(&pid) {
                return exit;
            }
            match self.rx.recv().unwrap() {
                Notice::Event(event) => {
                    self.history.push(event.data.clone());
                    event.resume(0);
                }
                Notice::Exit(pid, exit) => {
                    self.exits.insert(pid, exit);
                }
            }
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Ok(mut socket) = UnixStream::connect(self.dir.path().join("events")) {
            let _ = writeln!(socket, "STOP");
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

struct Build {
    pid: u32,
    worker: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl Build {
    fn wait(mut self, controller: &mut Controller) -> Exit {
        let exit = controller.wait(self.pid);
        self.worker.take().unwrap().join().unwrap();
        self.watchdog.take().unwrap().join().unwrap();
        exit
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        if self.worker.is_some() {
            kill_group(self.pid);
            let _ = self.worker.take().unwrap().join();
            let _ = self.watchdog.take().unwrap().join();
        }
    }
}

fn kill_group(pid: u32) {
    // Each test child leads its own process group, including rustc/build.rs.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn success(exit: Exit) {
    assert!(exit.status.success(), "{exit:?}");
}

#[expect(
    clippy::disallowed_methods,
    reason = "this observer intentionally exercises OS locks independently of Cargo guards"
)]
fn observer(path: &Path, blocked: bool) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let result = file.try_lock();
    if blocked {
        assert!(
            matches!(result, Err(TryLockError::WouldBlock)),
            "{path:?}: {result:?}"
        );
    } else {
        result.unwrap();
    }
}

fn lock_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.file_type().is_file()
            && matches!(
                entry.file_name().to_str(),
                Some(".lock" | ".cargo-unit-write-lock" | ".cargo-gc-lock")
            )
        {
            paths.push(entry.into_path());
        }
    }
    paths
}

fn native_command(p: &Project, target: &str) -> Command {
    p.cargo("build -j2")
        .arg("-Zfine-grain-locking")
        .arg("--target-dir")
        .arg(target)
        .masquerade_as_nightly_cargo(&["fine grain locking"])
        .env("INV_MODE", "A")
        .build_command()
}

fn writer_fixture(custom: bool, shared: &Path) -> Project {
    let build = if custom {
        "build = 'build.rs'"
    } else {
        "build = false"
    };
    project()
        .file(
            "Cargo.toml",
            "[package]\nname='consumer'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nprobe={path='probe'}\n[workspace]\n",
        )
        .file("src/main.rs", "fn main() { println!(\"{}\", probe::value()); }")
        .file(
            "probe/Cargo.toml",
            &format!("[package]\nname='probe'\nversion='0.1.0'\nedition='2024'\n{build}\n"),
        )
        .file(
            "probe/src/lib.rs",
            if custom {
                "pub fn value() -> &'static str { env!(\"GENERATED\") }"
            } else {
                "pub fn value() -> &'static str { option_env!(\"INV_MODE\").unwrap() }"
            },
        )
        .file(
            ".cargo/config.toml",
            &format!("[build]\nbuild-dir={:?}\n", shared.to_str().unwrap()),
        )
        .file(
            "probe/build.rs",
            r#"
            fn main() {
                use std::io::Write;
                let path = std::env::var_os("SCRIPT_COUNTER").unwrap();
                let mut count = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
                writeln!(count, "run").unwrap();
                println!("cargo:rerun-if-changed=build.rs");
                println!("cargo:rustc-env=GENERATED=A");
            }
            "#,
        )
        .build()
}

fn writer_outlives_conflict_budget(custom: bool) {
    let mut controller = Controller::new();
    let shared = paths::root().join("shared-build");
    let p = writer_fixture(custom, &shared);
    let wrapper = controller.dir.path().join("rustc-wrapper");
    let compile_count = controller.dir.path().join("compile-count");
    let script_count = controller.dir.path().join("script-count");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$COMPILE_COUNTER\"\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let command = |target: &str| {
        let mut command = native_command(&p, target);
        command
            .env("RUSTC_WRAPPER", &wrapper)
            .env("COMPILE_COUNTER", &compile_count)
            .env("SCRIPT_COUNTER", &script_count);
        command
    };
    let mode = if custom {
        "run-build-script"
    } else {
        "compile"
    };
    let first = controller.spawn(command("target-one"));
    let ready = controller.until(first.pid, "ReadyToRun", "probe", mode);
    let second = controller.spawn(command("target-two"));
    let waiter = controller.until(second.pid, "ReadyToRun", "probe", mode);
    assert_eq!(ready.lock(), waiter.lock());
    ready.resume(0);
    let writing = controller.until(first.pid, "WriterAcquired", "probe", mode);
    let unit = writing.lock();
    waiter.resume(0);
    controller
        .until(second.pid, "WaitingWriter", "probe", mode)
        .resume(10_000);
    let still_waiting = controller.until(second.pid, "WaitingWriter", "probe", mode);
    observer(&unit, true);
    writing.resume(0);
    // Expand the non-atomic downgrade gap. The gate must remain exclusive
    // even though the unit itself is momentarily unlocked.
    let gap = controller.until(first.pid, "DowngradeGap", "probe", mode);
    observer(&unit, false);
    let gate = unit.parent().unwrap().join(".cargo-unit-write-lock");
    observer(&gate, true);
    gap.resume(0);
    let validated = controller.until(first.pid, "SharedValidated", "probe", mode);
    observer(&gate, true);
    observer(&unit, true);
    validated.resume(0);
    let released = controller.until(first.pid, "GateReleased", "probe", mode);
    observer(&gate, false);
    observer(&unit, true);
    released.resume(0);
    success(first.wait(&mut controller));
    still_waiting.resume(10_000);
    controller
        .until(second.pid, "FreshReuse", "probe", mode)
        .resume(0);
    success(second.wait(&mut controller));
    let compiles = fs::read_to_string(&compile_count).unwrap();
    assert_eq!(
        compiles
            .lines()
            .filter(|l| l.contains("--crate-name probe "))
            .count(),
        1,
        "{compiles}"
    );
    if custom {
        assert_eq!(fs::read_to_string(script_count).unwrap(), "run\n");
    }
    for target in ["target-one", "target-two"] {
        let output = Command::new(p.root().join(target).join("debug/consumer"))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"A\n");
    }
    for path in lock_paths(&shared) {
        observer(&path, false);
    }
}

#[cargo_test]
fn legacy_script_ignores_actual_output_roots_but_not_similarly_named_sources() {
    use cargo_test_support::paths::CargoPathExt as _;
    let p = project()
        .file(
            "src/main.rs",
            r#"fn main() { println!("{}", env!("GENERATED")); }"#,
        )
        // Only the actual output roots are excluded, not every build-dir name.
        .file("src/build-dir/input", "A")
        .file(
            "build.rs",
            r#"
            fn main() {
                let value = std::fs::read_to_string("src/build-dir/input").unwrap();
                println!("cargo:rustc-env=GENERATED={value}");
            }
        "#,
        )
        .file(
            ".cargo/config.toml",
            "[build]\nbuild-dir='.generated/build'\n",
        )
        .build();
    let mut controller = Controller::new();
    let first = controller.spawn(native_command(&p, ".generated/artifacts"));
    controller
        .until(first.pid, "SharedValidated", "foo", "run-build-script")
        .resume(0);
    success(first.wait(&mut controller));
    run_binary(&p.root().join(".generated/artifacts/debug/foo"), b"A\n");

    let second = controller.spawn(native_command(&p, ".generated/artifacts"));
    let second_pid = second.pid;
    controller
        .until(second.pid, "PlanningFresh", "foo", "run-build-script")
        .resume(0);
    success(second.wait(&mut controller));
    assert!(
        !controller
            .history
            .iter()
            .any(|e| e["pid"] == second_pid && e["phase"] == "WriterAcquired")
    );

    // Establish the source/artifact timestamp relationship without waiting for
    // filesystem clock resolution or asserting any wall-clock duration.
    p.root().move_into_the_past();
    p.change_file("src/build-dir/input", "B");
    let third = controller.spawn(native_command(&p, ".generated/artifacts"));
    controller
        .until(third.pid, "WriterAcquired", "foo", "run-build-script")
        .resume(0);
    success(third.wait(&mut controller));
    run_binary(&p.root().join(".generated/artifacts/debug/foo"), b"B\n");
}

#[cargo_test]
fn ordinary_writer_outlives_reader_deadline() {
    writer_outlives_conflict_budget(false);
}

#[cargo_test]
fn build_script_writer_outlives_reader_deadline_and_replays_output() {
    writer_outlives_conflict_budget(true);
}

fn queued_probe_rechecks(scenario: &str) {
    let mut controller = Controller::new();
    let shared = paths::root().join("shared-build");
    let p = writer_fixture(false, &shared);
    if scenario == "external" {
        p.change_file("external", "A");
        p.change_file(
            "probe/src/lib.rs",
            "pub fn value() -> &'static str { include_str!(\"../../external\") }",
        );
    }
    let first_command = if scenario == "force" {
        p.cargo("check -p probe --lib")
            .arg("-Zfine-grain-locking")
            .arg("--target-dir=target-one")
            .masquerade_as_nightly_cargo(&["fine grain locking"])
            .env("INV_MODE", "A")
            .build_command()
    } else {
        native_command(&p, "target-one")
    };
    let first = controller.spawn(first_command);
    let ready = controller.until(first.pid, "ReadyToRun", "probe", "compile");
    let mut command = if scenario == "force" {
        p.cargo("fix -p probe --lib --allow-no-vcs")
            .arg("-Zfine-grain-locking")
            .arg("--target-dir=target-two")
            .masquerade_as_nightly_cargo(&["fine grain locking"])
            .env("INV_MODE", "A")
            .build_command()
    } else {
        native_command(&p, "target-two")
    };
    if scenario == "env" {
        command.env("INV_MODE", "B");
    }
    let second = controller.spawn(command);
    let waiting = controller.until(second.pid, "ReadyToRun", "probe", "compile");
    let unit = ready.lock().parent().unwrap().to_path_buf();
    assert_eq!(ready.lock(), waiting.lock());
    ready.resume(0);
    success(first.wait(&mut controller));
    match scenario {
        "external" => {
            // Explicitly make the reference old; no filesystem-clock wait.
            filetime::set_file_mtime(
                unit.join("fingerprint/dep-lib-probe"),
                filetime::FileTime::from_unix_time(1, 0),
            )
            .unwrap();
            p.change_file("external", "B");
        }
        "missing-output" => {
            let output = walkdir::WalkDir::new(unit.join("out"))
                .into_iter()
                .map(Result::unwrap)
                .find(|entry| entry.path().extension().is_some_and(|ext| ext == "rlib"))
                .unwrap();
            fs::remove_file(output.path()).unwrap();
        }
        "env" | "force" => {}
        _ => panic!("unknown scenario"),
    }
    let second_pid = second.pid;
    waiting.resume(0);
    controller
        .until(second_pid, "WriterAcquired", "probe", "compile")
        .resume(0);
    success(second.wait(&mut controller));
    assert!(
        !controller
            .history
            .iter()
            .any(|event| event["pid"] == second_pid
                && event["package"] == "probe"
                && event["phase"] == "FreshReuse")
    );
    if scenario != "force" {
        run_binary(
            &p.root().join("target-two/debug/consumer"),
            if matches!(scenario, "env" | "external") {
                b"B\n"
            } else {
                b"A\n"
            },
        );
    }
}

#[cargo_test]
fn queued_probe_reloads_latest_dep_info_environment() {
    queued_probe_rechecks("env");
}

#[cargo_test]
fn queued_probe_rechecks_external_file_mtime() {
    queued_probe_rechecks("external");
}

#[cargo_test]
fn queued_probe_rechecks_missing_outputs() {
    queued_probe_rechecks("missing-output");
}

#[cargo_test]
fn queued_probe_honors_force_rebuild_after_another_writer_finishes() {
    queued_probe_rechecks("force");
}

fn queued_probe_rechecks_checkout_marker(state: &str) {
    let mut controller = Controller::new();
    let home = controller.dir.path().join("cargo-home");
    let (p, wt, _) = abba_fixture();
    let first = controller.spawn(fork_command(&p, &p.root(), &home, "build -p a -j1", "A"));
    let ready = controller.until(first.pid, "ReadyToRun", "a", "compile");
    let second = controller.spawn(fork_command(&p, &wt, &home, "build -p a -j1", "A"));
    let waiting = controller.until(second.pid, "ReadyToRun", "a", "compile");
    assert_eq!(ready.lock(), waiting.lock());
    let marker = ready.lock().parent().unwrap().join(".cargo-checkout");
    ready.resume(0);
    success(first.wait(&mut controller));
    // A complete hash/output is insufficient if checkout policy denies reuse.
    fs::write(marker, format!("another-checkout\n{state}\n")).unwrap();
    waiting.resume(0);
    controller
        .until(second.pid, "WriterAcquired", "a", "compile")
        .resume(0);
    success(second.wait(&mut controller));
}

#[cargo_test]
fn queued_probe_rechecks_bound_checkout_marker() {
    queued_probe_rechecks_checkout_marker("bound");
}

#[cargo_test]
fn queued_probe_rechecks_unverified_checkout_marker() {
    queued_probe_rechecks_checkout_marker("unverified");
}

fn fork_command(p: &Project, cwd: &Path, home: &Path, args: &str, mode: &str) -> Command {
    p.cargo(args)
        .cwd(cwd)
        .env_remove("__CARGO_FORK_DEFAULTS_OPT_OUT")
        .env("CARGO_HOME", home)
        .env("CARGO_SHARED_DEPS", "0")
        .env("INV_MODE", mode)
        .build_command()
}

fn abba_fixture() -> (Project, PathBuf, PathBuf) {
    let p = git::new("foo", |p| {
        p.file(
            "Cargo.toml",
            "[workspace]\nmembers=['a','b','aux','active','consumer-a','consumer-b','gc-driver']\nresolver='3'\n",
        )
        .file(".gitignore", "target/\n")
        .file(
            "a/Cargo.toml",
            "[package]\nname='a'\nversion='0.1.0'\nedition='2024'\n",
        )
        .file("a/src/lib.rs", "pub fn mode() -> &'static str { option_env!(\"INV_MODE\").unwrap() }")
        .file(
            "b/Cargo.toml",
            "[package]\nname='b'\nversion='0.1.0'\nedition='2024'\n",
        )
        .file("b/src/lib.rs", "pub fn mode() -> &'static str { option_env!(\"INV_MODE\").unwrap() }")
        .file(
            "aux/Cargo.toml",
            "[package]\nname='aux'\nversion='0.1.0'\nedition='2024'\n",
        )
        .file("aux/src/lib.rs", "pub fn value() -> &'static str { \"aux\" }")
        .file(
            "active/Cargo.toml",
            "[package]\nname='active'\nversion='0.1.0'\nedition='2024'\n",
        )
        .file("active/src/lib.rs", "pub fn value() {}")
        .file(
            "active/build.rs",
            r#"
            fn main() {
                println!("cargo:rerun-if-changed=build.rs");
                let Some(path) = std::env::var_os("__CARGO_TEST_ACTIVE_CHILD_SOCKET") else { return };
                use std::io::{BufRead, Write};
                unsafe extern "C" { fn getppid() -> i32; }
                let mut socket = std::os::unix::net::UnixStream::connect(path).unwrap();
                let owner = unsafe { getppid() };
                writeln!(socket, "{{\"pid\":{owner},\"child_pid\":{},\"phase\":\"ActiveChild\",\"package\":\"active\",\"mode\":\"run-build-script\",\"target\":\"active\",\"lock\":\"\"}}", std::process::id()).unwrap();
                let mut reply = String::new();
                std::io::BufReader::new(socket).read_line(&mut reply).unwrap();
                println!("cargo:rustc-env=CHILD_FINISHED=yes");
            }
            "#,
        )
        .file(
            "consumer-a/Cargo.toml",
            "[package]\nname='consumer-a'\nversion='0.1.0'\nedition='2024'\n[dependencies]\na={path='../a'}\nb={path='../b'}\naux={path='../aux'}\nactive={path='../active'}\n",
        )
        .file(
            "consumer-a/src/main.rs",
            "fn main() { active::value(); println!(\"{} {} {}\", a::mode(), b::mode(), aux::value()); }",
        )
        .file(
            "consumer-b/Cargo.toml",
            "[package]\nname='consumer-b'\nversion='0.1.0'\nedition='2024'\n[dependencies]\na={path='../a'}\nb={path='../b'}\n",
        )
        .file(
            "consumer-b/src/main.rs",
            "fn main() { println!(\"{} {}\", a::mode(), b::mode()); }",
        )
        .file(
            "gc-driver/Cargo.toml",
            "[package]\nname='gc-driver'\nversion='0.1.0'\nedition='2024'\n",
        )
        .file("gc-driver/src/main.rs", "fn main() {}")
    });
    let repo = git2::Repository::open(p.root()).unwrap();
    let wt = paths::root().join("wt");
    let gc = paths::root().join("gc-wt");
    repo.worktree("wt", &wt, Some(&git2::WorktreeAddOptions::new()))
        .unwrap();
    repo.worktree("gc-wt", &gc, Some(&git2::WorktreeAddOptions::new()))
        .unwrap();
    (p, wt, gc)
}

fn planning_lock(controller: &Controller, pid: u32, package: &str, phase: &str) -> PathBuf {
    controller
        .history
        .iter()
        .find(|e| e["pid"] == pid && e["package"] == package && e["phase"] == phase)
        .unwrap_or_else(|| {
            panic!(
                "no retained reader for {pid}/{package}: {:?}",
                controller.history
            )
        })["lock"]
        .as_str()
        .unwrap()
        .into()
}

fn assert_packages_precede_units(controller: &Controller, pid: u32) {
    let events: Vec<_> = controller
        .history
        .iter()
        .filter(|event| event["pid"] == pid)
        .collect();
    let first_unit = events
        .iter()
        .position(|event| event["phase"] == "PlanningShared")
        .unwrap();
    let packages: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["phase"] == "PackageGcAcquired")
        .map(|(index, event)| {
            assert!(index < first_unit, "all packages must precede every unit");
            PathBuf::from(event["lock"].as_str().unwrap())
        })
        .collect();
    assert!(packages.len() >= 3, "{packages:?}");
    assert!(packages.windows(2).all(|pair| pair[0] < pair[1]));
    for event in events {
        if event["phase"] == "PlanningShared" {
            let unit = Path::new(event["lock"].as_str().unwrap());
            let package = unit
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(".cargo-gc-lock");
            assert!(packages.contains(&package), "{package:?}");
        }
    }
}

fn run_binary(path: &Path, expected: &[u8]) {
    let output = Command::new(path).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, expected);
}

#[cargo_test]
fn abba_selects_one_victim_releases_locks_and_survives_concurrent_gc() {
    use std::os::unix::fs::MetadataExt as _;
    let mut controller = Controller::new();
    let home = controller.dir.path().join("cargo-home");
    let (p, wt, gc) = abba_fixture();
    for (package, mode) in [("a", "A"), ("b", "B"), ("aux", "A")] {
        let output = fork_command(&p, &p.root(), &home, &format!("build -p {package}"), mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let victim = controller.spawn(fork_command(
        &p,
        &p.root(),
        &home,
        "build -p consumer-a -j1 --keep-going",
        "A",
    ));
    let victim_ready = controller.until(victim.pid, "ReadyToRun", "b", "compile");
    let survivor = controller.spawn(fork_command(&p, &wt, &home, "build -p consumer-b -j1", "B"));
    let survivor_ready = controller.until(survivor.pid, "ReadyToRun", "a", "compile");
    assert_packages_precede_units(&controller, victim.pid);
    assert_packages_precede_units(&controller, survivor.pid);
    let a_reader = planning_lock(&controller, victim.pid, "a", "PlanningFresh");
    let b_reader = planning_lock(&controller, survivor.pid, "b", "PlanningFresh");
    let auxiliary = planning_lock(&controller, victim.pid, "aux", "PlanningFresh");
    assert_eq!(victim_ready.lock(), b_reader);
    assert_eq!(survivor_ready.lock(), a_reader);
    let package_lock = auxiliary
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".cargo-gc-lock");
    let package_inode = fs::metadata(&package_lock).unwrap().ino();
    let queued = planning_lock(&controller, victim.pid, "consumer-a", "PlanningShared");
    let queued_inode = fs::metadata(&queued).unwrap().ino();
    let queued_package = queued
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".cargo-gc-lock");
    let queued_package_inode = fs::metadata(&queued_package).unwrap().ino();
    observer(&queued, false);
    observer(&queued_package, true);
    observer(&auxiliary, true);
    observer(&package_lock, true);
    victim_ready.resume(0);
    let victim_conflict = controller.until(victim.pid, "ConflictingReaders", "b", "compile");
    survivor_ready.resume(0);
    let survivor_conflict = controller.until(survivor.pid, "ConflictingReaders", "a", "compile");
    // Both EX attempts failed, neither retains a gate while waiting, and the
    // opposite process still retains the corresponding unit's SH.
    for unit in [&a_reader, &b_reader] {
        observer(unit, true);
        observer(
            &unit.parent().unwrap().join(".cargo-unit-write-lock"),
            false,
        );
    }
    let mut gc_command = fork_command(&p, &gc, &home, "build -p gc-driver", "GC");
    gc_command
        .env("CARGO_BUILD_GC_FREQUENCY", "always")
        .env("CARGO_BUILD_GC_MAX_UNUSED", "0 seconds")
        .env("CARGO_BUILD_GC_MAX_SIZE", "0");
    let collector = controller.spawn(gc_command);
    success(collector.wait(&mut controller));
    assert_eq!(fs::metadata(&queued).unwrap().ino(), queued_inode);
    assert_eq!(
        fs::metadata(&queued_package).unwrap().ino(),
        queued_package_inode
    );
    observer(&queued_package, true);
    assert_eq!(fs::metadata(&package_lock).unwrap().ino(), package_inode);
    observer(&auxiliary, true);
    victim_conflict.resume(2_000);
    let failed = victim.wait(&mut controller);
    assert!(!failed.status.success(), "{failed:?}");
    assert!(
        failed
            .stderr
            .contains("timed out waiting for conflicting readers"),
        "{failed:?}"
    );
    assert!(
        failed.stderr.contains("worktree") && failed.stderr.contains("retrying"),
        "{failed:?}"
    );
    // Only the victim used aux. Its package and unit locks must be available
    // while the survivor is still alive, not merely after both processes exit.
    observer(&auxiliary, false);
    observer(&package_lock, false);
    survivor_conflict.resume(0);
    let writing = controller.until(survivor.pid, "WriterAcquired", "a", "compile");
    assert_eq!(writing.lock(), a_reader);
    writing.resume(0);
    success(survivor.wait(&mut controller));
    run_binary(&wt.join("target/debug/consumer-b"), b"B B\n");
    let rerun = fork_command(&p, &p.root(), &home, "build -p consumer-a -j1", "A")
        .output()
        .unwrap();
    assert!(rerun.status.success(), "{rerun:?}");
    run_binary(&p.bin("consumer-a"), b"A A aux\n");
    for path in lock_paths(&home.join("targets")) {
        observer(&path, false);
    }
    assert_eq!(fs::metadata(package_lock).unwrap().ino(), package_inode);
}

#[cargo_test]
fn fatal_conflict_keeps_active_child_protected_even_with_keep_going() {
    let mut controller = Controller::new();
    let home = controller.dir.path().join("cargo-home");
    let (p, wt, _) = abba_fixture();
    for (package, mode) in [("a", "A"), ("b", "B"), ("aux", "A")] {
        let output = fork_command(&p, &p.root(), &home, &format!("build -p {package}"), mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let mut command = fork_command(
        &p,
        &p.root(),
        &home,
        "build -p consumer-a -j2 --keep-going",
        "A",
    );
    command.env(
        "__CARGO_TEST_ACTIVE_CHILD_SOCKET",
        controller.dir.path().join("events"),
    );
    let victim = controller.spawn(command);
    let victim_pid = victim.pid;
    // Hold b's ready event while allowing the active build-script executable
    // to compile. Neither dirty leaf may run before the opposing reader exists.
    let mut b_ready = None;
    let mut active_ready = None;
    while b_ready.is_none() || active_ready.is_none() {
        match controller.rx.recv().unwrap() {
            Notice::Event(event) => {
                controller.history.push(event.data.clone());
                match (
                    event.text("phase"),
                    event.text("package"),
                    event.text("mode"),
                ) {
                    ("ReadyToRun", "b", _) => b_ready = Some(event),
                    ("ReadyToRun", "active", "run-build-script") => active_ready = Some(event),
                    _ => event.resume(0),
                }
            }
            Notice::Exit(_, exit) => panic!("victim exited during planning: {exit:?}"),
        }
    }
    let survivor = controller.spawn(fork_command(&p, &wt, &home, "build -p consumer-b -j1", "B"));
    let survivor_ready = controller.until(survivor.pid, "ReadyToRun", "a", "compile");
    active_ready.unwrap().resume(0);
    let writing = controller.until(victim.pid, "WriterAcquired", "active", "run-build-script");
    let active_unit = writing.lock();
    let active_gate = active_unit.parent().unwrap().join(".cargo-unit-write-lock");
    let active_package = active_unit
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".cargo-gc-lock");
    writing.resume(0);
    let child = controller.until(victim.pid, "ActiveChild", "active", "run-build-script");
    let child_pid = child.data["child_pid"].as_u64().unwrap() as i32;
    b_ready.unwrap().resume(0);
    let victim_conflict = controller.until(victim.pid, "ConflictingReaders", "b", "compile");
    survivor_ready.resume(0);
    let survivor_conflict = controller.until(survivor.pid, "ConflictingReaders", "a", "compile");
    victim_conflict.resume(2_000);
    let fatal = controller.until(victim.pid, "FatalConflict", "b", "compile");
    for path in [&active_unit, &active_gate, &active_package] {
        observer(path, true);
    }
    fatal.resume(0);
    child.resume(0);
    let failed = victim.wait(&mut controller);
    assert!(!failed.status.success(), "{failed:?}");
    assert_eq!(
        unsafe { libc::kill(child_pid, 0) },
        -1,
        "child must be reaped"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    for path in [&active_unit, &active_gate, &active_package] {
        observer(path, false);
    }
    let fatal_index = controller
        .history
        .iter()
        .position(|e| e["pid"] == victim_pid && e["phase"] == "FatalConflict")
        .unwrap();
    assert!(
        !controller.history[fatal_index + 1..]
            .iter()
            .any(|e| e["pid"] == victim_pid && e["phase"] == "ReadyToRun"),
        "no new jobs after fatal cancellation"
    );
    survivor_conflict.resume(0);
    success(survivor.wait(&mut controller));
    for path in lock_paths(&home.join("targets")) {
        observer(&path, false);
    }
}

#[cargo_test]
fn failed_final_shared_validation_does_not_publish_success_or_repeat_work() {
    let mut controller = Controller::new();
    let shared = paths::root().join("shared-build");
    let p = writer_fixture(false, &shared);
    let counter = controller.dir.path().join("count");
    let wrapper = controller.dir.path().join("wrapper");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$COUNTER\"\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let command = || {
        let mut command = native_command(&p, "target-one");
        command
            .arg("--message-format=json")
            .env("RUSTC_WRAPPER", &wrapper)
            .env("COUNTER", &counter);
        command
    };
    let build = controller.spawn(command());
    let gap = controller.until(build.pid, "DowngradeGap", "probe", "compile");
    let unit = gap.lock();
    let fingerprint = walkdir::WalkDir::new(unit.parent().unwrap())
        .into_iter()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "lib-probe")
        .unwrap()
        .into_path();
    fs::write(&fingerprint, "").unwrap();
    gap.resume(0);
    let failed = build.wait(&mut controller);
    assert!(!failed.status.success(), "{failed:?}");
    assert!(
        failed.stderr.contains("changed while finalizing"),
        "{failed:?}"
    );
    let messages: Vec<Value> = failed
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !messages
            .iter()
            .any(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "probe")
    );
    assert!(
        messages
            .iter()
            .any(|m| m["reason"] == "build-finished" && m["success"] == false)
    );
    let count = || {
        fs::read_to_string(&counter)
            .unwrap()
            .lines()
            .filter(|line| line.contains("--crate-name probe "))
            .count()
    };
    assert_eq!(count(), 1, "the consumed work must not be invoked again");
    for path in lock_paths(&shared) {
        observer(&path, false);
    }
    let rerun = command().output().unwrap();
    assert!(rerun.status.success(), "{rerun:?}");
    assert_eq!(
        count(),
        2,
        "an incomplete fingerprint must rebuild on retry"
    );
    run_binary(&p.root().join("target-one/debug/consumer"), b"A\n");
}

#[cargo_test]
fn compiler_failure_releases_writer_and_never_reuses_unfinished_fingerprint() {
    let mut controller = Controller::new();
    let shared = paths::root().join("shared-build");
    let p = writer_fixture(false, &shared);
    fs::write(
        p.root().join("probe/src/lib.rs"),
        "compile_error!(\"intentional failure\");",
    )
    .unwrap();
    let first = controller.spawn(native_command(&p, "target-one"));
    let ready = controller.until(first.pid, "ReadyToRun", "probe", "compile");
    let second = controller.spawn(native_command(&p, "target-two"));
    let second_ready = controller.until(second.pid, "ReadyToRun", "probe", "compile");
    ready.resume(0);
    let writing = controller.until(first.pid, "WriterAcquired", "probe", "compile");
    let unit = writing.lock();
    second_ready.resume(0);
    let waiter = controller.until(second.pid, "WaitingWriter", "probe", "compile");
    writing.resume(0);
    let failed = first.wait(&mut controller);
    assert!(
        !failed.status.success() && failed.stderr.contains("intentional failure"),
        "{failed:?}"
    );
    observer(&unit, false);
    observer(
        &unit.parent().unwrap().join(".cargo-unit-write-lock"),
        false,
    );
    let fingerprint = walkdir::WalkDir::new(unit.parent().unwrap())
        .into_iter()
        .map(Result::unwrap)
        .find(|e| e.file_name() == "lib-probe")
        .unwrap()
        .into_path();
    assert!(fs::read(&fingerprint).unwrap().is_empty());
    waiter.resume(10_000);
    let retry = controller.until(second.pid, "WriterAcquired", "probe", "compile");
    assert_eq!(retry.lock(), unit);
    retry.resume(0);
    let failed = second.wait(&mut controller);
    assert!(
        !failed.status.success() && failed.stderr.contains("intentional failure"),
        "{failed:?}"
    );
    assert!(
        !controller
            .history
            .iter()
            .any(|e| e["phase"] == "FreshReuse")
    );
    for path in lock_paths(&shared) {
        observer(&path, false);
    }
    fs::write(
        p.root().join("probe/src/lib.rs"),
        "pub fn value() -> &'static str { option_env!(\"INV_MODE\").unwrap() }",
    )
    .unwrap();
    let rerun = native_command(&p, "target-one").output().unwrap();
    assert!(rerun.status.success(), "{rerun:?}");
    run_binary(&p.root().join("target-one/debug/consumer"), b"A\n");
}

#[cargo_test]
fn pipelining_notifies_metadata_before_validated_completion() {
    let mut controller = Controller::new();
    let p = writer_fixture(false, &paths::root().join("shared-build"));
    fs::write(
        p.root().join("Cargo.toml"),
        "[package]\nname='consumer'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nmiddle={path='middle'}\n[workspace]\n",
    ).unwrap();
    fs::create_dir_all(p.root().join("middle/src")).unwrap();
    fs::write(
        p.root().join("middle/Cargo.toml"),
        "[package]\nname='middle'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nprobe={path='../probe'}\n",
    ).unwrap();
    fs::write(
        p.root().join("middle/src/lib.rs"),
        "pub fn value() -> &'static str { probe::value() }",
    )
    .unwrap();
    fs::write(
        p.root().join("src/main.rs"),
        "fn main() { println!(\"{}\", middle::value()); }",
    )
    .unwrap();
    let command = || {
        let mut command = native_command(&p, "target-one");
        command.arg("--message-format=json");
        command
    };
    let build = controller.spawn(command());
    let pid = build.pid;
    let metadata = controller.until(pid, "MetadataProduced", "probe", "compile");
    let unit = metadata.lock();
    observer(&unit, true);
    metadata.resume(0);
    let finished_work = controller.until(pid, "WorkFinished", "probe", "compile");
    let files: Vec<_> = walkdir::WalkDir::new(unit.parent().unwrap())
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(
        files
            .iter()
            .any(|e| e.path().extension().is_some_and(|ext| ext == "rlib"))
    );
    assert!(
        files
            .iter()
            .any(|e| e.file_name() == "lib-probe" && e.metadata().unwrap().len() > 0)
    );
    if !controller
        .history
        .iter()
        .any(|e| e["pid"] == pid && e["phase"] == "ReadyToRun" && e["package"] == "middle")
    {
        controller
            .until(pid, "ReadyToRun", "middle", "compile")
            .resume(0);
    }
    assert!(
        !controller
            .history
            .iter()
            .any(|e| e["pid"] == pid && e["phase"] == "JobCompleted" && e["package"] == "probe")
    );
    observer(&unit.parent().unwrap().join(".cargo-unit-write-lock"), true);
    finished_work.resume(0);
    success(build.wait(&mut controller));
    let index = |phase: &str, package: &str| {
        controller
            .history
            .iter()
            .position(|e| e["pid"] == pid && e["phase"] == phase && e["package"] == package)
            .unwrap()
    };
    assert!(index("MetadataProduced", "probe") < index("WorkFinished", "probe"));
    assert!(index("WorkFinished", "probe") < index("SharedValidated", "probe"));
    assert!(index("SharedValidated", "probe") < index("JobCompleted", "probe"));
    assert!(index("ReadyToRun", "middle") < index("JobCompleted", "probe"));
    let start = controller.history.len();
    let replay = controller.spawn(command());
    let exit = replay.wait(&mut controller);
    assert!(exit.status.success(), "{exit:?}");
    assert!(
        !controller.history[start..]
            .iter()
            .any(|e| e["phase"] == "WriterAcquired")
    );
    let artifacts: Vec<Value> = exit
        .stdout
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|m| m["reason"] == "compiler-artifact")
        .collect();
    let mut targets: Vec<_> = artifacts
        .iter()
        .map(|m| m["target"]["name"].as_str().unwrap())
        .collect();
    targets.sort_unstable();
    assert_eq!(targets, ["consumer", "middle", "probe"]);
    for artifact in &artifacts {
        let root = match artifact["target"]["name"].as_str().unwrap() {
            "consumer" => p.root(),
            package => p.root().join(package),
        };
        assert_eq!(
            artifact["manifest_path"],
            root.join("Cargo.toml").to_str().unwrap()
        );
        assert!(!artifact["filenames"].as_array().unwrap().is_empty());
    }
    assert!(artifacts.iter().all(|m| m["fresh"] == true));
    run_binary(&p.root().join("target-one/debug/consumer"), b"A\n");
}

#[cfg(target_os = "linux")]
#[cargo_test]
#[ignore = "manual diagnostic: set CARGO_UNIT_LOCK_BASELINE_BIN to an unmodified baseline Cargo"]
fn baseline_abba_kernel_wait_graph() {
    use std::os::unix::fs::MetadataExt as _;
    let baseline = std::env::var_os("CARGO_UNIT_LOCK_BASELINE_BIN").unwrap();
    let mut controller = Controller::new();
    let home = controller.dir.path().join("cargo-home");
    let (p, wt, _) = abba_fixture();
    let command = |cwd: &Path, args: &str, mode: &str| {
        let original = fork_command(&p, cwd, &home, args, mode);
        let mut command = Command::new(&baseline);
        command.args(original.get_args()).current_dir(cwd);
        for (key, value) in original.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    };
    for (package, mode) in [("a", "A"), ("b", "B"), ("aux", "A"), ("active", "A")] {
        let output = command(&p.root(), &format!("build -p {package}"), mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    let shim = controller.dir.path().join("flock-hook.so");
    let source = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("crates/cargo-test-support/fixtures/unit-lock-flock-hook.c");
    let compiled = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&shim)
        .arg(source)
        .arg("-ldl")
        .output()
        .unwrap();
    assert!(compiled.status.success(), "{compiled:?}");
    let mut first_command = command(&p.root(), "build -p consumer-a -j1", "A");
    first_command.env("LD_PRELOAD", &shim);
    let first = controller.spawn(first_command);
    let first_ex = controller.until(first.pid, "BaselineEX", "b", "compile");
    let mut second_command = command(&wt, "build -p consumer-b -j1", "B");
    second_command.env("LD_PRELOAD", &shim);
    let second = controller.spawn(second_command);
    let second_ex = controller.until(second.pid, "BaselineEX", "a", "compile");
    let a_inode = fs::metadata(second_ex.lock()).unwrap().ino();
    let b_inode = fs::metadata(first_ex.lock()).unwrap().ino();
    first_ex.resume(0);
    second_ex.resume(0);
    // The unmodified binary cannot signal after entering blocking flock.
    // Linux's lock table is the authoritative external observation instead.
    // Polling has no elapsed-time assertion; the child watchdog is cleanup only.
    loop {
        let table = fs::read_to_string("/proc/locks").unwrap();
        let entries: Vec<_> = table
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                let i = fields.iter().position(|s| *s == "FLOCK")?;
                Some((
                    line,
                    fields.get(i + 3)?.parse::<u32>().ok()?,
                    fields.get(i + 4)?.rsplit(':').next()?.parse::<u64>().ok()?,
                    fields.contains(&"->"),
                    *fields.get(i + 2)?,
                ))
            })
            .collect();
        let lock = |pid, inode, waiting, mode| {
            entries
                .iter()
                .any(|entry| (entry.1, entry.2, entry.3, entry.4) == (pid, inode, waiting, mode))
        };
        if lock(first.pid, a_inode, false, "READ")
            && lock(first.pid, b_inode, true, "WRITE")
            && lock(second.pid, b_inode, false, "READ")
            && lock(second.pid, a_inode, true, "WRITE")
        {
            eprintln!("BASELINE_ABBA_KERNEL_WAIT_GRAPH");
            for entry in entries
                .iter()
                .filter(|e| e.1 == first.pid || e.1 == second.pid)
            {
                eprintln!("{}", entry.0);
            }
            break;
        }
        match controller.rx.try_recv() {
            Ok(Notice::Exit(pid, exit)) => panic!("baseline {pid} exited before cycle: {exit:?}"),
            Ok(Notice::Event(event)) => event.resume(0),
            Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
            Err(error) => panic!("{error}"),
        }
    }
    // The cycle, not a lack-of-progress timeout, proved the old deadlock.
    drop(first);
    drop(second);
    for path in lock_paths(&home.join("targets")) {
        observer(&path, false);
    }
}

// Compatibility regressions run the real fork defaults, including content keys,
// with each test's isolated Cargo home (never the developer's shared pool).
fn compatibility_cargo(p: &Project, args: &str) -> cargo_test_support::Execs {
    let mut exec = p.cargo(args);
    exec.env_remove("__CARGO_FORK_DEFAULTS_OPT_OUT");
    exec
}

#[cargo_test]
fn legacy_probe_preserves_bindeps_configuration() {
    use cargo_test_support::paths::CargoPathExt as _;
    let p = project()
        .file("Cargo.toml", r#"
            [package]
            name = "foo"
            version = "0.1.0"
            edition = "2021"
            [build-dependencies]
            tool = { path = "tool", artifact = "bin" }
            [workspace]
        "#)
        .file("src/lib.rs", "pub fn foo() {}")
        .file("build.rs", r#"fn main() { assert!(std::path::Path::new(&std::env::var_os("CARGO_BIN_FILE_TOOL").unwrap()).is_file()); }"#)
        .file("tool/Cargo.toml", r#"[package]
            name = "tool"
            version = "0.1.0"
            edition = "2021"
        "#)
        .file("tool/src/main.rs", "fn main() {}")
        .build();
    for _ in 0..2 {
        compatibility_cargo(&p, "build -Zbindeps")
            .masquerade_as_nightly_cargo(&["bindeps"])
            .run();
    }
    p.root().move_into_the_past();
    p.change_file("build.rs", r#"fn main() { assert!(std::path::Path::new(&std::env::var_os("CARGO_BIN_FILE_TOOL").unwrap()).is_file()); /* touched */ }"#);
    compatibility_cargo(&p, "build -Zbindeps")
        .masquerade_as_nightly_cargo(&["bindeps"])
        .run();
}

#[cargo_test]
fn legacy_probe_preserves_cli_registry_configuration() {
    use cargo_test_support::registry::{Package, RegistryBuilder};
    let registry = RegistryBuilder::new()
        .alternative()
        .no_configure_registry()
        .build();
    Package::new("dep", "1.0.0").alternative(true).publish();
    let p = project()
        .file(
            "Cargo.toml",
            r#"
            [package]
            name = "foo"
            version = "0.1.0"
            edition = "2021"
            [dependencies]
            dep = { version = "1.0.0", registry = "test-registry" }
        "#,
        )
        .file("src/lib.rs", "pub fn foo() {}")
        .file("build.rs", "fn main() {}")
        .build();
    let config = format!(
        "registries.test-registry.index={:?}",
        registry.index_url().as_str()
    );
    for _ in 0..2 {
        compatibility_cargo(&p, "build")
            .arg("--config")
            .arg(&config)
            .run();
    }
}

#[cargo_test]
fn fixing_sources_completes_without_publishing_the_old_content_key() {
    let original = "pub fn foo() -> i32 { let mut x = 1; x }";
    let p = project().file("src/lib.rs", original).build();
    compatibility_cargo(&p, "fix --allow-no-vcs --lib").run();
    assert!(!p.read_file("src/lib.rs").contains("let mut x"));
    compatibility_cargo(&p, "build --lib").run();
    let output = compatibility_cargo(&p, "build --lib --message-format=json")
        .build_command()
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let artifacts: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|m| m["reason"] == "compiler-artifact")
        .collect();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["target"]["name"], "foo");
    assert_eq!(artifacts[0]["fresh"], true);
    // The original source hashes to the key cargo fix ran under. Its fixed
    // check output must not be reusable from that pre-edit content key.
    p.change_file("src/lib.rs", original);
    compatibility_cargo(&p, "check --lib")
        .with_stderr_contains("[WARNING] variable does not need to be mutable")
        .run();
}

#[cargo_test]
fn print_cfg_completes_and_is_not_reused_as_a_compilation() {
    let p = project().file("src/lib.rs", "pub fn foo() {} ").build();
    for _ in 0..2 {
        compatibility_cargo(&p, "rustc --lib -- --print=cfg")
            .with_stdout_contains("target_arch=\"[..]\"")
            .run();
    }
    compatibility_cargo(&p, "build --lib").run();
    assert!(p.root().join("target/debug/libfoo.rlib").is_file());
}

#[cargo_test]
fn unpretty_completes_and_is_not_reused_as_a_compilation() {
    let p = project().file("src/lib.rs", "pub fn foo() {} ").build();
    for _ in 0..2 {
        compatibility_cargo(&p, "rustc --lib -- -Zunpretty=expanded")
            .env("RUSTC_BOOTSTRAP", "1")
            .with_stdout_contains("pub fn foo() {}")
            .run();
    }
    compatibility_cargo(&p, "build --lib").run();
    assert!(p.root().join("target/debug/libfoo.rlib").is_file());
}

#[cargo_test]
fn raw_rustdoc_json_completes_and_does_not_reuse_html_fingerprint() {
    let p = project().file("src/lib.rs", "pub fn foo() {} ").build();
    for _ in 0..2 {
        // Prove the invocation ran again, not merely that an old JSON remains.
        let json = p.root().join("target/doc/foo.json");
        if json.exists() {
            fs::remove_file(&json).unwrap();
        }
        compatibility_cargo(
            &p,
            "rustdoc --lib -- -Zunstable-options --output-format=json",
        )
        .env("RUSTC_BOOTSTRAP", "1")
        .run();
        let document: Value = serde_json::from_slice(&fs::read(json).unwrap()).unwrap();
        assert!(document["index"].is_object());
    }
    compatibility_cargo(&p, "doc --lib --no-deps").run();
    assert!(p.root().join("target/doc/foo/index.html").is_file());
}

#[cargo_test]
fn fixing_sources_without_content_keys_still_validates_completion() {
    let p = project()
        .file("src/lib.rs", "pub fn foo() -> i32 { let mut x = 1; x }")
        .build();
    compatibility_cargo(&p, "fix --allow-no-vcs --lib")
        .env("CARGO_SHARE_WORKSPACE_UNITS", "0")
        .run();
    assert!(!p.read_file("src/lib.rs").contains("let mut x"));
    compatibility_cargo(&p, "build --lib")
        .env("CARGO_SHARE_WORKSPACE_UNITS", "0")
        .run();
}

#[cargo_test]
fn missing_json_output_is_not_a_successful_completion() {
    let p = project().file("src/lib.rs", "pub fn foo() {} ").build();
    let mut controller = Controller::new();
    let build = controller.spawn(
        compatibility_cargo(
            &p,
            "rustdoc --lib -- -Zunstable-options --output-format=json",
        )
        .env("RUSTC_BOOTSTRAP", "1")
        .build_command(),
    );
    let written = controller.until(build.pid, "WorkFinished", "foo", "compile");
    fs::remove_file(p.root().join("target/doc/foo.json")).unwrap();
    written.resume(0);
    let exit = build.wait(&mut controller);
    assert!(!exit.status.success(), "{exit:?}");
    assert!(
        exit.stderr.contains("unit changed while finalizing"),
        "{exit:?}"
    );
    assert!(
        !controller
            .history
            .iter()
            .any(|e| e["phase"] == "SharedValidated")
    );
}

#[cargo_test]
fn legacy_probe_enumerates_files_added_after_planning() {
    let p = project()
        .file("src/lib.rs", "pub fn foo() {}")
        .file("build.rs", "fn main() {} ")
        .build();
    let mut controller = Controller::new();
    let build = controller.spawn(native_command(&p, "target-one"));
    let written = controller.until(build.pid, "WorkFinished", "foo", "run-build-script");
    // Establish a file timestamp relation, not an elapsed-time requirement.
    // A planning-time file list would miss this new input and falsely validate.
    let newest = ["Cargo.toml", "Cargo.lock", "src/lib.rs", "build.rs"]
        .iter()
        .map(|path| {
            filetime::FileTime::from_last_modification_time(
                &fs::metadata(p.root().join(path)).unwrap(),
            )
            .unix_seconds()
        })
        .max()
        .unwrap();
    p.change_file("src/new-input", "added after the probe was prepared");
    filetime::set_file_mtime(
        p.root().join("src/new-input"),
        filetime::FileTime::from_unix_time(newest + 1, 0),
    )
    .unwrap();
    written.resume(0);
    let exit = build.wait(&mut controller);
    assert!(!exit.status.success(), "{exit:?}");
    assert!(
        exit.stderr.contains("unit changed while finalizing"),
        "{exit:?}"
    );
    assert!(
        !controller
            .history
            .iter()
            .any(|e| e["phase"] == "SharedValidated" && e["mode"] == "run-build-script")
    );
}
