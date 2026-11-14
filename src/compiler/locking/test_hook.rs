//! Opt-in synchronization, unavailable in release builds. Replies supply
//! virtual elapsed milliseconds for waits, never a production clock value.

use crate::CargoResult;
use crate::compiler::{BuildRunner, Unit};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Default)]
pub(in crate::compiler) struct TestHook {
    #[cfg(all(unix, debug_assertions))]
    data: Option<std::sync::Arc<Data>>,
}

#[cfg(all(unix, debug_assertions))]
struct Data {
    socket: std::path::PathBuf,
    path: std::path::PathBuf,
    package: String,
    target: String,
    mode: &'static str,
}

impl TestHook {
    pub fn new(build_runner: &BuildRunner<'_, '_>, unit: &Unit, path: &Path) -> Self {
        #[cfg(all(unix, debug_assertions))]
        {
            let data = build_runner
                .bcx
                .gctx
                .get_env_os("__CARGO_TEST_UNIT_LOCK_SOCKET")
                .map(|socket| {
                    std::sync::Arc::new(Data {
                        socket: socket.into(),
                        path: path.into(),
                        package: unit.pkg.name().to_string(),
                        target: unit.target.name().to_string(),
                        mode: if unit.mode.is_run_custom_build() {
                            "run-build-script"
                        } else {
                            "compile"
                        },
                    })
                });
            Self { data }
        }
        #[cfg(not(all(unix, debug_assertions)))]
        {
            let _ = (build_runner, unit, path);
            Self::default()
        }
    }

    pub fn event(&self, phase: &str) -> CargoResult<Option<Duration>> {
        #[cfg(all(unix, debug_assertions))]
        if let Some(data) = &self.data {
            use anyhow::Context as _;
            use std::io::{BufRead as _, Write as _};
            let mut socket = std::os::unix::net::UnixStream::connect(&data.socket)
                .context("failed to connect unit-lock test controller")?;
            let event = serde_json::json!({
                "pid": std::process::id(), "phase": phase, "lock": data.path,
                "package": data.package, "target": data.target, "mode": data.mode,
            });
            writeln!(socket, "{event}")?;
            let mut reply = String::new();
            std::io::BufReader::new(socket).read_line(&mut reply)?;
            let millis: u64 = reply
                .trim()
                .parse()
                .context("invalid unit-lock test clock reply")?;
            return Ok(Some(Duration::from_millis(millis)));
        }
        let _ = phase;
        Ok(None)
    }
}
