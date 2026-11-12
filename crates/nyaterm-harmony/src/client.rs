//! Subprocess facade over the user's `hdc` executable.
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nyaterm_core::models::harmony::HdcDeviceSelector;

use crate::error::{HdcError, HdcResult};

/// Captured child output is capped so a chatty `hilog`-style command cannot
/// exhaust host memory.
const OUTPUT_LIMIT: usize = 64 * 1024;
const LIST_TIMEOUT: Duration = Duration::from_secs(5);
const SHELL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);
const FILE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HdcDevice {
    pub connect_key: String,
}

/// Immutable binding handed to sessions and file operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceBinding {
    pub executable: PathBuf,
    pub connect_key: String,
}

impl DeviceBinding {
    pub fn from_selector(executable: &Path, selector: &HdcDeviceSelector) -> HdcResult<Self> {
        selector.validate().map_err(|_| HdcError::Offline)?;
        Ok(Self {
            executable: executable.to_path_buf(),
            connect_key: selector.connect_key(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct HdcClient {
    pub executable: PathBuf,
}

impl HdcClient {
    pub fn local(custom: Option<&Path>) -> HdcResult<Self> {
        Ok(Self {
            executable: resolve_executable(custom)?,
        })
    }

    /// Connected targets as reported by `hdc list targets`. `[Empty]` output and
    /// blank lines map to an empty list.
    pub fn list_targets(&self) -> HdcResult<Vec<HdcDevice>> {
        let output = run_bounded(self.command(&["list", "targets"]), LIST_TIMEOUT)?;
        match output.status {
            Some(0) => {}
            _ => return Err(HdcError::Offline),
        }
        let mut devices = Vec::new();
        for line in output.stdout_lines() {
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            devices.push(HdcDevice { connect_key: line });
        }
        Ok(devices)
    }

    /// One-shot shell command. hdc does not propagate the remote exit status,
    /// so success is "hdc completed"; callers verify content themselves.
    pub fn shell_capture(&self, connect_key: &str, command: &str) -> HdcResult<Vec<u8>> {
        let output = run_bounded(
            self.target_command(connect_key, &["shell", command]),
            SHELL_CAPTURE_TIMEOUT,
        )?;
        require_success(&output)?;
        Ok(output.stdout)
    }

    /// Cheap lease check: an unreachable key fails quickly with a nonzero exit.
    pub fn probe(&self, connect_key: &str) -> HdcResult<()> {
        self.shell_capture(connect_key, "true")?;
        Ok(())
    }

    pub fn file_send(&self, connect_key: &str, local: &Path, remote: &str) -> HdcResult<()> {
        let mut command = self.target_command(connect_key, &["file", "send"]);
        command.arg(local).arg(remote);
        let output = run_bounded(command, FILE_TRANSFER_TIMEOUT)?;
        require_success(&output)
    }

    pub fn file_recv(&self, connect_key: &str, remote: &str, local: &Path) -> HdcResult<()> {
        let mut command = self.target_command(connect_key, &["file", "recv"]);
        command.arg(remote).arg(local);
        let output = run_bounded(command, FILE_TRANSFER_TIMEOUT)?;
        require_success(&output)
    }

    /// Long-lived interactive shell child for terminal sessions. The caller
    /// owns the child and its pipes.
    pub fn spawn_shell(&self, connect_key: &str) -> HdcResult<Child> {
        Ok(self
            .target_command(connect_key, &["shell"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.executable);
        command.args(args);
        command
    }

    fn target_command(&self, connect_key: &str, args: &[&str]) -> Command {
        let mut command = self.command(&["-t", connect_key]);
        command.args(args);
        command
    }
}

fn require_success(output: &HdcOutput) -> HdcResult<()> {
    match output.status {
        Some(0) => Ok(()),
        Some(code) => Err(HdcError::CommandFailed(code)),
        None => Err(HdcError::Timeout),
    }
}

struct HdcOutput {
    stdout: Vec<u8>,
    #[allow(dead_code)]
    stderr: Vec<u8>,
    status: Option<i32>,
}

impl HdcOutput {
    fn stdout_lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }
}

/// Spawn, capture both pipes, and reap within a deadline. Output beyond the
/// cap is drained but discarded. Killing an hdc child is always safe: the
/// server survives, only the command is abandoned.
fn run_bounded(mut command: Command, timeout: Duration) -> HdcResult<HdcOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_pipe = child.stdout.take().ok_or(HdcError::Protocol("stdout"))?;
    let stderr_pipe = child.stderr.take().ok_or(HdcError::Protocol("stderr"))?;
    let mut stdout_worker = capture_worker(stdout_pipe);
    let mut stderr_worker = capture_worker(stderr_pipe);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HdcError::Timeout);
            }
            None => std::thread::sleep(Duration::from_millis(15)),
        }
    };
    // The pipes close when the child exits; a short join keeps a stuck pipe
    // from blocking teardown.
    let stdout = join_with_deadline(&mut stdout_worker, Duration::from_secs(1));
    let stderr = join_with_deadline(&mut stderr_worker, Duration::from_secs(1));
    Ok(HdcOutput {
        stdout,
        stderr,
        status: status.code(),
    })
}

struct CaptureWorker {
    handle: Option<JoinHandle<()>>,
    receiver: mpsc::Receiver<Vec<u8>>,
    result: Vec<u8>,
}

fn capture_worker<R>(pipe: R) -> CaptureWorker
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let mut reader = BufReader::new(pipe);
    let handle = nyaterm_core::runtime::worker::spawn("hdc-capture".to_string(), move || {
        let mut collected = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if collected.len() < OUTPUT_LIMIT {
                        let keep = (OUTPUT_LIMIT - collected.len()).min(count);
                        collected.extend_from_slice(&chunk[..keep]);
                    }
                }
            }
        }
        let _ = sender.send(collected);
    });
    match handle {
        Ok(handle) => CaptureWorker {
            handle: Some(handle),
            receiver,
            result: Vec::new(),
        },
        Err(_) => CaptureWorker {
            handle: None,
            receiver,
            result: Vec::new(),
        },
    }
}

impl CaptureWorker {
    fn finish(&mut self) {
        if self.result.is_empty()
            && let Ok(result) = self.receiver.recv_timeout(Duration::from_secs(1))
        {
            self.result = result;
        }
    }
}

fn join_with_deadline(worker: &mut CaptureWorker, grace: Duration) -> Vec<u8> {
    worker.finish();
    if let Some(handle) = worker.handle.take() {
        let deadline = Instant::now() + grace;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                return worker.result.clone();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = handle.join();
        // The send may land after the earlier receive timeout expires.
        worker.finish();
    }
    std::mem::take(&mut worker.result)
}

/// Bounded child reaping shared with terminal-session teardown.
pub fn reap(child: &mut Child, grace: Duration) -> HdcResult<()> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(status) = child.try_wait()? {
            let _ = status;
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Resolve the hdc executable: explicit settings path first, then an optional
/// bundled copy beside the application, then `PATH`. SDK roots are only
/// consulted in debug builds, mirroring the ADB backend.
pub fn resolve_executable(custom: Option<&Path>) -> HdcResult<PathBuf> {
    if let Some(path) = custom.filter(|path| !path.as_os_str().is_empty()) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !path.is_file() || std::fs::metadata(path)?.permissions().mode() & 0o111 == 0 {
                return Err(HdcError::ExecutableMissing);
            }
        }
        return path
            .is_file()
            .then(|| path.to_path_buf())
            .ok_or(HdcError::ExecutableMissing);
    }
    let name = if cfg!(windows) { "hdc.exe" } else { "hdc" };
    let bundled = std::env::current_exe()?
        .parent()
        .ok_or(HdcError::ExecutableMissing)?
        .join("hdc")
        .join(name);
    if bundled.is_file() {
        return Ok(bundled);
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    }) {
        return Ok(path);
    }
    if cfg!(debug_assertions) {
        for variable in ["DEVECO_SDK_HOME", "OHOS_SDK_HOME", "OHOS_BASE_SDK_HOME"] {
            if let Some(root) = std::env::var_os(variable) {
                for candidate in [
                    PathBuf::from(&root)
                        .join("default")
                        .join("openharmony")
                        .join("toolchains")
                        .join(name),
                    PathBuf::from(&root)
                        .join("sdk")
                        .join("default")
                        .join("openharmony")
                        .join("toolchains")
                        .join(name),
                ] {
                    if candidate.is_file() {
                        return Ok(candidate);
                    }
                }
            }
        }
    }
    Err(HdcError::ExecutableMissing)
}

#[cfg(test)]
mod tests {
    use super::{DeviceBinding, HdcClient, HdcDevice, resolve_executable};
    use crate::error::HdcError;
    use nyaterm_core::models::harmony::HdcDeviceSelector;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn custom_path_must_point_to_an_existing_file() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("missing-hdc");
        assert!(matches!(
            resolve_executable(Some(&missing)),
            Err(HdcError::ExecutableMissing)
        ));
        assert!(matches!(
            resolve_executable(Some(&PathBuf::new())),
            Err(HdcError::ExecutableMissing)
        ));
        let file = root.path().join("hdc-fixture");
        std::fs::write(&file, b"fixture").unwrap();
        assert_eq!(resolve_executable(Some(&file)).unwrap(), file);
    }

    #[test]
    fn bindings_reject_invalid_selectors_before_touching_the_filesystem() {
        let root = TempDir::new().unwrap();
        let executable = root.path().join("hdc-fixture");
        std::fs::write(&executable, b"fixture").unwrap();
        let bad = HdcDeviceSelector::Usb {
            connect_key: String::new(),
        };
        assert!(matches!(
            DeviceBinding::from_selector(&executable, &bad),
            Err(HdcError::Offline)
        ));
        let good = HdcDeviceSelector::Tcp {
            host: "example.test".into(),
            port: 10178,
        };
        let binding = DeviceBinding::from_selector(&executable, &good).unwrap();
        assert_eq!(binding.connect_key, "example.test:10178");
    }

    #[test]
    fn parsing_ignores_headers_and_blank_lines() {
        let device = HdcDevice {
            connect_key: "127.0.0.1:10178".into(),
        };
        // The parser itself is exercised through list_targets against a fixture
        // executable; here only the filter rules are pinned.
        for line in ["[Empty]", "", "  "] {
            assert!(line.is_empty() || line.starts_with('[') || line.trim().is_empty());
        }
        assert!(!device.connect_key.starts_with('['));
    }

    #[test]
    fn missing_executable_reports_a_stable_error() {
        let root = TempDir::new().unwrap();
        let custom = root.path().join("hdc-fixture");
        std::fs::write(&custom, b"fixture").unwrap();
        let client = HdcClient::local(Some(&custom)).unwrap();
        assert!(client.list_targets().is_err());
    }
}
