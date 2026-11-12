//! HarmonyOS file operations over the user's `hdc` executable.
//!
//! hdc has no structured file protocol, so browsing is implemented as bounded
//! `hdc shell` commands wrapped in sentinels (`@@nyaterm-missing@@`, …): hdc does
//! not forward the remote exit status, and matching English diagnostics is
//! locale-dependent. Binary content moves through `hdc file send/recv` only.
//!
//! The service mirrors [`crate::adb::AdbFileService`] so the file-browser and
//! transfer layers can treat both device backends the same way.

mod copy;
mod listing;
mod transfer;

pub(crate) use copy::execute as execute_copy;
pub(crate) use transfer::HdcTransferEvent;

use crate::remote_file::{
    RemoteTextDocument, RemoteTextMetadata, RemoteTextRevision, RemoteTextWriteResult,
};
use crate::sftp::{
    RemoteBinaryFile, RemoteFilePath, SftpFileEntry, SftpFileProperties, SftpFileType,
    SftpRemoteTextFile, SftpTransferControl,
};
use nyaterm_harmony::client::{DeviceBinding, HdcClient, HdcShellOutput};
use nyaterm_harmony::error::HdcError;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Sentinels are echoed by the *device* shell, so they must be valid in every
/// POSIX shell and unlikely to collide with a real filename.
const MISSING: &str = "@@nyaterm-missing@@";
const FAILED: &str = "@@nyaterm-failed@@";
const EXISTS: &str = "@@nyaterm-exists@@";

/// Redacts device output from an hdc failure, mirroring `AdbFileError`.
#[derive(Debug, thiserror::Error)]
#[error("HarmonyOS file operation failed")]
pub struct HdcFileError;

/// Local validation failures carry no paths or device-controlled output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HdcFileValidationError {
    #[error("HarmonyOS requires an absolute path without NUL")]
    InvalidPath,
    #[error("Invalid HarmonyOS raw path token")]
    InvalidPathToken,
    #[error("Invalid HarmonyOS filename")]
    InvalidFilename,
}

/// Policy failures are stable and contain no device or filesystem data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HdcFilePolicyError {
    #[error("HarmonyOS target attributes are unavailable")]
    AttributesUnavailable,
    #[error("HarmonyOS file timestamps cannot be preserved")]
    TimestampUnavailable,
    #[error("HarmonyOS edit exceeds the size limit")]
    EditLimitExceeded,
    #[error("A revision is required for a safe edit save")]
    RevisionRequired,
}

impl HdcFilePolicyError {
    pub fn code(self) -> &'static str {
        match self {
            Self::AttributesUnavailable => "AttributesUnavailable",
            Self::TimestampUnavailable => "TimestampUnavailable",
            Self::EditLimitExceeded => "EditLimitExceeded",
            Self::RevisionRequired => "RevisionRequired",
        }
    }
}

pub(crate) fn file_error(error: anyhow::Error) -> anyhow::Error {
    error.context(HdcFileError)
}

/// The hdc surface file browsing needs. It exists so tests can drive the service
/// without spawning a process per command.
pub trait HdcFileExecutor: Send + Sync + std::fmt::Debug {
    fn shell(
        &self,
        command: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<HdcShellOutput, HdcError>;
    fn send(
        &self,
        local: &Path,
        remote: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HdcError>;
    fn recv(
        &self,
        remote: &str,
        local: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HdcError>;
}

#[derive(Debug)]
struct ClientExecutor {
    client: HdcClient,
    connect_key: String,
}

impl HdcFileExecutor for ClientExecutor {
    fn shell(
        &self,
        command: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<HdcShellOutput, HdcError> {
        self.client
            .shell_capture_output_with_cancel(&self.connect_key, command, cancelled)
    }

    fn send(
        &self,
        local: &Path,
        remote: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HdcError> {
        self.client
            .file_send_with_cancel(&self.connect_key, local, remote, cancelled)
    }

    fn recv(
        &self,
        remote: &str,
        local: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HdcError> {
        self.client
            .file_recv_with_cancel(&self.connect_key, remote, local, cancelled)
    }
}

/// The subset of `ls -l` metadata the browser needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdcFileStat {
    pub file_type: SftpFileType,
    pub size: u64,
    pub modified: u64,
    pub mode: u32,
}

impl HdcFileStat {
    pub fn is_directory(self) -> bool {
        self.file_type == SftpFileType::Directory
    }
}

/// A HarmonyOS file browser bound to one device.
///
/// Cloning shares the same cancellation state, exactly like `AdbFileService`.
#[derive(Clone)]
pub struct HdcFileService {
    executor: Arc<dyn HdcFileExecutor>,
    /// hdc connect key (`serial` for USB, `host:port` for TCP).
    pub connect_key: String,
    pub control: SftpTransferControl,
    valid: Arc<AtomicBool>,
}

impl std::fmt::Debug for HdcFileService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HdcFileService")
            .field("device", &self.connect_key)
            .finish()
    }
}

impl HdcFileService {
    pub fn new(binding: &DeviceBinding) -> Self {
        Self::with_executor(
            Arc::new(ClientExecutor {
                client: HdcClient {
                    executable: binding.executable.clone(),
                },
                connect_key: binding.connect_key.clone(),
            }),
            binding.connect_key.clone(),
        )
    }

    /// Testing seam: run the same command protocol against a fake device.
    pub fn with_executor(executor: Arc<dyn HdcFileExecutor>, connect_key: String) -> Self {
        Self {
            executor,
            connect_key,
            control: SftpTransferControl::new(),
            valid: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn cancel(&self) {
        self.control.cancel();
        self.valid.store(false, Ordering::Release);
    }

    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }

    /// Two services address the same device when they share a connect key.
    pub fn same_device(&self, other: &Self) -> bool {
        self.connect_key == other.connect_key
    }

    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.is_valid(), HdcFileError);
        self.control.check_cancelled()?;
        Ok(())
    }

    /// Wait out a pause, honoring cancellation from both the service and the
    /// caller's transfer control.
    pub(crate) fn check_transfer(&self, control: &SftpTransferControl) -> anyhow::Result<()> {
        loop {
            anyhow::ensure!(self.is_valid(), HdcFileError);
            self.control.check_cancelled()?;
            control.check_cancelled()?;
            if !control.is_paused() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn cancellation<'a>(&'a self, control: &'a SftpTransferControl) -> impl Fn() -> bool + 'a {
        move || self.control.is_cancelled() || control.is_cancelled() || !self.is_valid()
    }

    /// Run a device shell command and return its raw stdout.
    pub(crate) fn shell(
        &self,
        command: &str,
        control: &SftpTransferControl,
    ) -> anyhow::Result<Vec<u8>> {
        self.check_transfer(control)?;
        let cancel = self.cancellation(control);
        let output = self.executor.shell(command, &cancel).map_err(hdc_error)?;
        control.check_cancelled()?;
        Ok(output.stdout)
    }

    pub fn child_path(
        &self,
        parent: &RemoteFilePath,
        name: &str,
    ) -> anyhow::Result<RemoteFilePath> {
        anyhow::ensure!(
            !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']),
            HdcFileValidationError::InvalidFilename
        );
        let mut raw = raw_path(parent)?;
        if raw.last() != Some(&b'/') {
            raw.push(b'/');
        }
        raw.extend_from_slice(name.as_bytes());
        Ok(RemoteFilePath::from_raw(
            String::from_utf8_lossy(&raw),
            &raw,
        ))
    }

    pub fn stat(&self, path: &RemoteFilePath) -> anyhow::Result<HdcFileStat> {
        self.stat_with_control(path, &self.control)
    }

    pub(crate) fn stat_with_control(
        &self,
        path: &RemoteFilePath,
        control: &SftpTransferControl,
    ) -> anyhow::Result<HdcFileStat> {
        let raw = raw_path(path)?;
        let command = format!(
            "ls -ld {target} 2>/dev/null || echo {MISSING}",
            target = quote(&raw)?,
        );
        let output = self.shell(&command, control)?;
        let lines = output_lines(&output);
        if lines.iter().any(|line| line == MISSING) {
            anyhow::bail!(HdcError::FileMissing);
        }
        let entry = lines
            .iter()
            .find_map(|line| listing::parse_line(line))
            .ok_or(HdcError::Protocol("stat"))?;
        if entry.file_type == SftpFileType::Symlink {
            // `ls -ld` reports the link itself; browse what it points at.
            return self.stat_following(raw, control);
        }
        Ok(HdcFileStat {
            file_type: entry.file_type,
            size: entry.size.unwrap_or(0),
            modified: u64::from(entry.modified_at.unwrap_or(0)),
            mode: entry.mode.unwrap_or(0o644),
        })
    }

    fn stat_following(
        &self,
        raw: Vec<u8>,
        control: &SftpTransferControl,
    ) -> anyhow::Result<HdcFileStat> {
        let command = format!(
            "ls -ldL {target} 2>/dev/null || echo {MISSING}",
            target = quote(&raw)?,
        );
        let output = self.shell(&command, control)?;
        let lines = output_lines(&output);
        if lines.iter().any(|line| line == MISSING) {
            anyhow::bail!(HdcError::FileMissing);
        }
        let entry = lines
            .iter()
            .find_map(|line| listing::parse_line(line))
            .ok_or(HdcError::Protocol("stat"))?;
        Ok(HdcFileStat {
            file_type: entry.file_type,
            size: entry.size.unwrap_or(0),
            modified: u64::from(entry.modified_at.unwrap_or(0)),
            mode: entry.mode.unwrap_or(0o644),
        })
    }

    pub fn exists(&self, path: &RemoteFilePath) -> anyhow::Result<bool> {
        match self.stat(path) {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.downcast_ref::<HdcError>(),
                    Some(HdcError::FileMissing)
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub fn list_dir_path(&self, path: &RemoteFilePath) -> anyhow::Result<Vec<SftpFileEntry>> {
        self.list_dir_path_with_control(path, &self.control)
    }

    pub(crate) fn list_dir_path_with_control(
        &self,
        path: &RemoteFilePath,
        control: &SftpTransferControl,
    ) -> anyhow::Result<Vec<SftpFileEntry>> {
        let raw = raw_path(path)?;
        let command = format!(
            "ls -l {target} 2>/dev/null || echo {MISSING}",
            target = quote(&raw)?,
        );
        let output = self.shell(&command, control)?;
        let lines = output_lines(&output);
        if lines.iter().any(|line| line == MISSING) {
            anyhow::bail!(HdcError::FileMissing);
        }
        let mut entries = Vec::new();
        let mut unparsed = 0_usize;
        for line in &lines {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("total ") {
                continue;
            }
            let Some(entry) = listing::parse_line(trimmed) else {
                unparsed += 1;
                continue;
            };
            let mut child = raw.clone();
            if child.last() != Some(&b'/') {
                child.push(b'/');
            }
            child.extend_from_slice(entry.name.as_bytes());
            let child = RemoteFilePath::from_raw(String::from_utf8_lossy(&child), &child);
            entries.push(SftpFileEntry {
                name: entry.name,
                path: child.display_path,
                raw_path_token: child.raw_path_token,
                file_type: entry.file_type,
                size: entry.size,
                permissions: entry.mode,
                modified_at: entry.modified_at,
                owner: entry.owner,
                group: entry.group,
                symlink_target_is_directory: false,
            });
        }
        // A single unparsable line (a shell warning) is tolerated; a directory
        // where nothing parses is a protocol change, not an empty directory.
        anyhow::ensure!(
            !entries.is_empty() || unparsed == 0,
            HdcError::Protocol("listing")
        );
        Ok(entries)
    }

    pub fn remote_file_properties(
        &self,
        path: &RemoteFilePath,
    ) -> anyhow::Result<SftpFileProperties> {
        self.remote_file_properties_with_control(path, &self.control)
    }

    pub(crate) fn remote_file_properties_with_control(
        &self,
        path: &RemoteFilePath,
        control: &SftpTransferControl,
    ) -> anyhow::Result<SftpFileProperties> {
        let stat = self.stat_with_control(path, control)?;
        Ok(SftpFileProperties {
            name: file_name(&path.display_path),
            path: path.display_path.clone(),
            file_type: stat.file_type,
            size: Some(stat.size),
            permissions: Some(stat.mode),
            permissions_symbolic: format!("{:o}", stat.mode & 0o7777),
            owner: String::new(),
            group: String::new(),
            uid: None,
            gid: None,
            modified_at: u32::try_from(stat.modified).ok(),
            accessed_at: None,
            raw_path_token: path.raw_path_token.clone(),
            symlink_target: None,
            symlink_target_is_directory: false,
        })
    }

    /// Run a command whose only success signal is the absence of [`FAILED`].
    fn command(&self, command: &str, control: &SftpTransferControl) -> anyhow::Result<()> {
        let output = self.shell(command, control)?;
        let lines = output_lines(&output);
        if lines.iter().any(|line| line == FAILED) {
            anyhow::bail!(HdcError::Rejected);
        }
        Ok(())
    }

    pub fn create_dir_remote_path(
        &self,
        path: &RemoteFilePath,
        _mode: Option<u32>,
    ) -> anyhow::Result<()> {
        let command = format!(
            "mkdir -p {target} 2>/dev/null || echo {FAILED}",
            target = quote(&raw_path(path)?)?,
        );
        self.command(&command, &self.control)
    }

    pub fn create_file_remote_path(
        &self,
        path: &RemoteFilePath,
        _mode: Option<u32>,
    ) -> anyhow::Result<()> {
        let target = quote(&raw_path(path)?)?;
        let command = format!("[ -e {target} ] || : > {target} 2>/dev/null || echo {FAILED}");
        self.command(&command, &self.control)
    }

    pub fn delete_remote_path(&self, path: &RemoteFilePath) -> anyhow::Result<()> {
        let raw = raw_path(path)?;
        let components: Vec<&[u8]> = raw
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty() && *part != b".")
            .collect();
        anyhow::ensure!(
            !components.is_empty() && !components.contains(&b"..".as_slice()),
            "refusing root or ambiguous HarmonyOS deletion"
        );
        let command = format!(
            "rm -rf {target} 2>/dev/null || echo {FAILED}",
            target = quote(&raw)?,
        );
        self.command(&command, &self.control)
    }

    pub fn rename_remote_paths(
        &self,
        from: &RemoteFilePath,
        to: &RemoteFilePath,
    ) -> anyhow::Result<()> {
        let command = format!(
            "mv {from} {to} 2>/dev/null || echo {FAILED}",
            from = quote(&raw_path(from)?)?,
            to = quote(&raw_path(to)?)?,
        );
        self.command(&command, &self.control)
    }

    pub fn rename_remote_paths_no_replace(
        &self,
        from: &RemoteFilePath,
        to: &RemoteFilePath,
    ) -> anyhow::Result<()> {
        let from = quote(&raw_path(from)?)?;
        let to = quote(&raw_path(to)?)?;
        let command = format!(
            "if [ -e {to} ] || [ -L {to} ]; then echo {EXISTS}; \
             else mv {from} {to} 2>/dev/null || echo {FAILED}; fi",
        );
        let output = self.shell(&command, &self.control)?;
        let lines = output_lines(&output);
        if lines.iter().any(|line| line == EXISTS) {
            anyhow::bail!(crate::file_browser::FileTransferEntryError::TargetExists);
        }
        if lines.iter().any(|line| line == FAILED) {
            anyhow::bail!(HdcError::Rejected);
        }
        Ok(())
    }

    pub fn read_file_bytes_path(
        &self,
        path: &RemoteFilePath,
        limit: u64,
    ) -> anyhow::Result<RemoteBinaryFile> {
        let stat = self.stat(path)?;
        anyhow::ensure!(
            stat.file_type == SftpFileType::File,
            crate::file_browser::FileTransferEntryError::UnsupportedFileType
        );
        anyhow::ensure!(stat.size <= limit, "HarmonyOS preview exceeds limit");
        let temporary = tempfile::NamedTempFile::new()?;
        let control = SftpTransferControl::new();
        self.receive_file(path, temporary.path(), &control, |_| {})?;
        let content_bytes = std::fs::read(temporary.path())?;
        anyhow::ensure!(
            content_bytes.len() as u64 <= limit,
            "HarmonyOS preview exceeds limit"
        );
        Ok(RemoteBinaryFile {
            path: path.display_path.clone(),
            content_bytes,
            size: stat.size,
            modified_at: stat.modified,
        })
    }

    pub fn read_text_file_path(
        &self,
        path: &RemoteFilePath,
        limit: u64,
    ) -> anyhow::Result<SftpRemoteTextFile> {
        let file = self.read_file_bytes_path(path, limit)?;
        Ok(SftpRemoteTextFile {
            path: file.path,
            content: String::from_utf8(file.content_bytes)?,
            size: file.size,
            modified_at: file.modified_at,
        })
    }

    pub fn read_text_document_path(
        &self,
        path: &RemoteFilePath,
        limit: u64,
    ) -> anyhow::Result<RemoteTextDocument> {
        let file = self.read_text_file_path(path, limit)?;
        let revision = RemoteTextRevision::from_bytes(
            file.content.as_bytes(),
            RemoteTextMetadata {
                size: file.size,
                modified_at: Some(file.modified_at),
            },
        );
        Ok(RemoteTextDocument {
            path: file.path,
            content: file.content,
            revision,
        })
    }

    pub fn write_text_document_path(
        &self,
        path: &RemoteFilePath,
        content: impl AsRef<str>,
        expected: Option<&RemoteTextRevision>,
        force: bool,
    ) -> anyhow::Result<RemoteTextWriteResult> {
        let content = content.as_ref();
        anyhow::ensure!(
            content.len() <= 16 * 1024 * 1024,
            HdcFilePolicyError::EditLimitExceeded
        );
        anyhow::ensure!(
            force || expected.is_some(),
            HdcFilePolicyError::RevisionRequired
        );
        if let Some(expected) = expected {
            let current = self.read_text_document_path(path, 16 * 1024 * 1024)?;
            if current.revision != *expected {
                return Ok(RemoteTextWriteResult::Conflict);
            }
        }
        let mut local = tempfile::NamedTempFile::new()?;
        {
            use std::io::Write as _;
            local.write_all(content.as_bytes())?;
            local.flush()?;
        }
        self.upload_with_options(
            local.path(),
            path,
            true,
            &SftpTransferControl::new(),
            &crate::sftp_transfer_types::SftpTransferOptions::default(),
            |_| {},
        )?;
        let stat = self.stat(path)?;
        anyhow::ensure!(
            stat.size == content.len() as u64,
            crate::file_browser::FileTransferEntryError::SourceChanged
        );
        Ok(RemoteTextWriteResult::Saved {
            revision: RemoteTextRevision::from_bytes(
                content.as_bytes(),
                RemoteTextMetadata {
                    size: stat.size,
                    modified_at: Some(stat.modified),
                },
            ),
        })
    }

    /// Reject copies that would read and write the same path on one device.
    pub(crate) fn validate_copy_destination(
        &self,
        source: &RemoteFilePath,
        target: &Self,
        destination: &RemoteFilePath,
        _directory: bool,
    ) -> anyhow::Result<()> {
        if !self.same_device(target) {
            return Ok(());
        }
        let source = normalized_path(&raw_path(source)?)?;
        let destination = normalized_path(&raw_path(destination)?)?;
        anyhow::ensure!(source != destination, "source and destination overlap");
        Ok(())
    }
}

fn hdc_error(error: HdcError) -> anyhow::Error {
    match error {
        HdcError::Cancelled => anyhow::anyhow!(crate::sftp::SFTP_TRANSFER_CANCELLED),
        other => other.into(),
    }
}

pub(crate) fn raw_path(path: &RemoteFilePath) -> anyhow::Result<Vec<u8>> {
    let raw = path
        .raw_path()
        .map_err(|_| HdcFileValidationError::InvalidPathToken)?
        .unwrap_or_else(|| path.display_path.as_bytes().to_vec());
    anyhow::ensure!(
        raw.starts_with(b"/") && !raw.contains(&0),
        HdcFileValidationError::InvalidPath
    );
    Ok(raw)
}

/// Single-quote a raw path for the device shell. NUL cannot reach a shell word.
pub(crate) fn quote(raw: &[u8]) -> anyhow::Result<String> {
    let text = std::str::from_utf8(raw).map_err(|_| HdcFileValidationError::InvalidPath)?;
    anyhow::ensure!(!text.contains('\0'), HdcFileValidationError::InvalidPath);
    Ok(format!("'{}'", text.replace('\'', r"'\''")))
}

fn output_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim_end_matches('\r').trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn file_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("/")
        .to_string()
}

fn normalized_path(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        raw.starts_with(b"/") && !raw.contains(&0),
        HdcFileValidationError::InvalidPath
    );
    let mut components = Vec::new();
    for component in raw.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                components.pop();
            }
            value => components.push(value),
        }
    }
    let mut result = vec![b'/'];
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            result.push(b'/');
        }
        result.extend_from_slice(component);
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
