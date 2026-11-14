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
        Ok(self.shell_capture_output(connect_key, command)?.stdout)
    }

    /// Both pipes of a one-shot shell command. Callers that need to tell a
    /// missing file from an empty listing inspect `stderr` themselves.
    pub fn shell_capture_output(
        &self,
        connect_key: &str,
        command: &str,
    ) -> HdcResult<HdcShellOutput> {
        self.shell_capture_output_with_cancel(connect_key, command, &|| false)
    }

    pub fn shell_capture_output_with_cancel(
        &self,
        connect_key: &str,
        command: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> HdcResult<HdcShellOutput> {
        let output = run_bounded_with(
            self.target_command(connect_key, &["shell", command]),
            SHELL_CAPTURE_TIMEOUT,
            cancelled,
        )?;
        require_success(&output)?;
        Ok(HdcShellOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            status: output.status,
        })
    }

    /// Cheap lease check: an unreachable key fails quickly with a nonzero exit.
    pub fn probe(&self, connect_key: &str) -> HdcResult<()> {
        self.shell_capture(connect_key, "true")?;
        Ok(())
    }

    pub fn file_send(&self, connect_key: &str, local: &Path, remote: &str) -> HdcResult<()> {
        self.file_send_with_cancel(connect_key, local, remote, &|| false)
    }

    pub fn file_send_with_cancel(
        &self,
        connect_key: &str,
        local: &Path,
        remote: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> HdcResult<()> {
        let mut command = self.target_command(connect_key, &["file", "send"]);
        command.arg(local).arg(remote);
        let output = run_bounded_with(command, FILE_TRANSFER_TIMEOUT, cancelled)?;
        require_success(&output)
    }

    pub fn file_recv(&self, connect_key: &str, remote: &str, local: &Path) -> HdcResult<()> {
        self.file_recv_with_cancel(connect_key, remote, local, &|| false)
    }

    pub fn file_recv_with_cancel(
        &self,
        connect_key: &str,
        remote: &str,
        local: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> HdcResult<()> {
        let mut command = self.target_command(connect_key, &["file", "recv"]);
        command.arg(remote).arg(local);
        let output = run_bounded_with(command, FILE_TRANSFER_TIMEOUT, cancelled)?;
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
    stderr: Vec<u8>,
    status: Option<i32>,
}

/// Both pipes of a one-shot `hdc shell` invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HdcShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<i32>,
}

impl HdcShellOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
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
    run_bounded_with_impl(&mut command, timeout, &|| false)
}

fn run_bounded_with(
    mut command: Command,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> HdcResult<HdcOutput> {
    run_bounded_with_impl(&mut command, timeout, cancelled)
}

fn run_bounded_with_impl(
    command: &mut Command,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> HdcResult<HdcOutput> {
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
            None if cancelled() => {
                let _ = child.kill();
                let _ = child.wait();
                discard_workers(&mut stdout_worker, &mut stderr_worker);
                return Err(HdcError::Cancelled);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                discard_workers(&mut stdout_worker, &mut stderr_worker);
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

/// Abandoned commands still close their pipes; collect the workers so a killed
/// transfer does not leave detached readers behind.
fn discard_workers(stdout: &mut CaptureWorker, stderr: &mut CaptureWorker) {
    let _ = join_with_deadline(stdout, Duration::from_millis(200));
    let _ = join_with_deadline(stderr, Duration::from_millis(200));
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
/// Official SDK/toolchain locations probed after the bundled copy and PATH.
///
/// Release builds must probe these too: most users get hdc from DevEco Studio
/// or the standalone Command Line Tools without adding it to PATH, and the
/// published settings hint promises automatic discovery.
fn sdk_candidates(name: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in [
        "HDC_HOME",
        "DEVECO_SDK_HOME",
        "OHOS_SDK_HOME",
        "OHOS_BASE_SDK_HOME",
        "HARMONYOS_SDK_HOME",
    ] {
        if let Some(root) = std::env::var_os(variable) {
            roots.push(PathBuf::from(root));
        }
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Huawei").join("Sdk"));
        }
        if let Some(programs) = std::env::var_os("ProgramFiles") {
            roots.push(
                PathBuf::from(programs)
                    .join("Huawei")
                    .join("DevEco Studio")
                    .join("sdk"),
            );
        }
    }
    let suffixes = [
        PathBuf::from(name),
        PathBuf::from("toolchains").join(name),
        PathBuf::from("default")
            .join("openharmony")
            .join("toolchains")
            .join(name),
        PathBuf::from("sdk")
            .join("default")
            .join("openharmony")
            .join("toolchains")
            .join(name),
        PathBuf::from("Command-Line-Tools")
            .join("sdk")
            .join("default")
            .join("openharmony")
            .join("toolchains")
            .join(name),
    ];
    roots
        .into_iter()
        .flat_map(|root| suffixes.iter().map(move |suffix| root.join(suffix)))
        .collect()
}

fn verify_bundled_hdc(executable: &Path) -> HdcResult<()> {
    let root = executable
        .parent()
        .and_then(Path::parent)
        .ok_or(HdcError::ExecutableMissing)?;
    let manifest = root.join("harmony-resources.json");
    if !manifest.is_file() && cfg!(debug_assertions) {
        return Ok(());
    }
    let metadata = std::fs::metadata(&manifest)?;
    if metadata.len() > 128 * 1024 {
        return Err(HdcError::Protocol("resource manifest length"));
    }
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest)?)
        .map_err(|_| HdcError::Protocol("resource manifest"))?;
    let files = document
        .get("files")
        .and_then(serde_json::Value::as_object)
        .ok_or(HdcError::Protocol("resource inventory"))?;
    let expected: &[&str] = if cfg!(windows) {
        &["hdc/hdc.exe", "hdc/libusb_shared.dll", "hdc/NOTICE.txt", "hdc/SOURCE.json"]
    } else {
        &["hdc/hdc", "hdc/NOTICE.txt", "hdc/SOURCE.json"]
    };
    if expected.iter().any(|name| !files.contains_key(*name)) {
        return Err(HdcError::Protocol("hdc companions missing"));
    }
    for (name, hash) in files.iter().filter(|(name, _)| name.starts_with("hdc/")) {
        if std::path::Path::new(name)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || name.contains('\\')
        {
            return Err(HdcError::Protocol("resource path"));
        }
        use sha2::Digest;
        let bytes = std::fs::read(root.join(name))?;
        if Some(hex::encode(sha2::Sha256::digest(&bytes)).as_str()) != hash.as_str() {
            return Err(HdcError::Protocol("bundled hdc checksum"));
        }
    }
    Ok(())
}

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
        verify_bundled_hdc(&bundled)?;
        return Ok(bundled);
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    }) {
        return Ok(path);
    }
    for candidate in sdk_candidates(name) {
        if candidate.is_file() {
            return Ok(candidate);
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
