//! Single-file transfers over `hdc file send/recv`.
//!
//! hdc reports no byte-level progress, so a transfer reports its size once the
//! child exits. Staging keeps a failed transfer from destroying the target:
//! uploads land in a sibling `.nyaterm-*.part` file and downloads in a temporary
//! file next to the destination.

use crate::download_path::{
    DownloadTemporary, root_for_target, target_exists, writable_file_permissions,
};
use crate::file_browser::FileTransferEntryError;
use crate::hdc::{
    FAILED, HdcFilePolicyError, HdcFileService, HdcFileStat, hdc_error, quote, raw_path,
};
use crate::sftp::{RemoteFilePath, SftpFileType, SftpTransferControl};
use crate::sftp_transfer_types::{FileTransferPhase, SftpTransferOptions};
use std::path::Path;

pub(crate) enum HdcTransferEvent {
    Data(u64),
    Phase(FileTransferPhase),
}

/// A staged upload removes its temporary file unless it was committed.
struct StagedUpload<'a> {
    service: &'a HdcFileService,
    temporary: RemoteFilePath,
    committed: bool,
}

impl Drop for StagedUpload<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(raw) = raw_path(&self.temporary)
            && let Ok(target) = quote(&raw)
        {
            let _ = self.service.shell(
                &format!("rm -f {target} 2>/dev/null || true"),
                &SftpTransferControl::new(),
            );
        }
    }
}

impl HdcFileService {
    /// Remote path as an hdc argument: hdc takes operands as process arguments,
    /// so names that are not valid UTF-8 cannot be addressed.
    fn transfer_argument(&self, path: &RemoteFilePath) -> anyhow::Result<String> {
        let raw = raw_path(path)?;
        std::str::from_utf8(&raw)
            .map(str::to_string)
            .map_err(|_| anyhow::anyhow!("HarmonyOS path is not valid UTF-8"))
    }

    pub(crate) fn receive_file(
        &self,
        path: &RemoteFilePath,
        local: &Path,
        control: &SftpTransferControl,
        mut progress: impl FnMut(u64),
    ) -> anyhow::Result<u64> {
        self.check_transfer(control)?;
        let remote = self.transfer_argument(path)?;
        let cancel = self.cancellation(control);
        self.executor
            .recv(&remote, local, &cancel)
            .map_err(hdc_error)?;
        let bytes = std::fs::metadata(local)?.len();
        progress(bytes);
        Ok(bytes)
    }

    pub fn download(
        &self,
        path: &RemoteFilePath,
        local: &Path,
        replace: bool,
        control: &SftpTransferControl,
        mut progress: impl FnMut(u64),
    ) -> anyhow::Result<u64> {
        self.download_with_options(
            path,
            local,
            replace,
            control,
            &SftpTransferOptions::default(),
            move |event| {
                if let HdcTransferEvent::Data(bytes) = event {
                    progress(bytes);
                }
            },
        )
    }

    pub(crate) fn download_with_options(
        &self,
        path: &RemoteFilePath,
        local: &Path,
        replace: bool,
        control: &SftpTransferControl,
        options: &SftpTransferOptions,
        mut event: impl FnMut(HdcTransferEvent),
    ) -> anyhow::Result<u64> {
        event(HdcTransferEvent::Phase(FileTransferPhase::Transferring));
        let before = self.stat_with_control(path, control)?;
        anyhow::ensure!(
            before.file_type == SftpFileType::File,
            FileTransferEntryError::UnsupportedFileType
        );
        anyhow::ensure!(
            !options.preserve_timestamps,
            HdcFilePolicyError::TimestampUnavailable
        );
        let root = root_for_target(local);
        writable_file_permissions(root, local)?;
        if !replace {
            anyhow::ensure!(!target_exists(local)?, FileTransferEntryError::TargetExists);
        }
        let (temporary, output) = DownloadTemporary::create(root, local)?;
        let bytes = self.receive_file(path, temporary.path(), control, |bytes| {
            event(HdcTransferEvent::Data(bytes))
        })?;
        output.sync_all()?;
        anyhow::ensure!(bytes == before.size, FileTransferEntryError::SourceChanged);
        let after = self.stat_with_control(path, control)?;
        same_content(before, after)?;
        event(HdcTransferEvent::Phase(FileTransferPhase::Committing));
        drop(output);
        temporary.commit(root, local)?;
        Ok(bytes)
    }

    pub fn upload(
        &self,
        local: &Path,
        target: &RemoteFilePath,
        replace: bool,
        control: &SftpTransferControl,
        mut progress: impl FnMut(u64),
    ) -> anyhow::Result<u64> {
        self.upload_with_options(
            local,
            target,
            replace,
            control,
            &SftpTransferOptions::default(),
            move |event| {
                if let HdcTransferEvent::Data(bytes) = event {
                    progress(bytes);
                }
            },
        )
    }

    pub(crate) fn upload_with_options(
        &self,
        local: &Path,
        target: &RemoteFilePath,
        replace: bool,
        control: &SftpTransferControl,
        options: &SftpTransferOptions,
        mut event: impl FnMut(HdcTransferEvent),
    ) -> anyhow::Result<u64> {
        event(HdcTransferEvent::Phase(FileTransferPhase::Transferring));
        let before = std::fs::symlink_metadata(local)?;
        anyhow::ensure!(
            before.is_file(),
            FileTransferEntryError::UnsupportedFileType
        );
        anyhow::ensure!(
            !options.preserve_timestamps,
            HdcFilePolicyError::TimestampUnavailable
        );
        if !replace && self.exists_with_control(target, control)? {
            anyhow::bail!(FileTransferEntryError::TargetExists);
        }
        let parent = target.parent()?;
        let temporary =
            self.child_path(&parent, &format!(".nyaterm-{}.part", uuid::Uuid::new_v4()))?;
        let mut staged = StagedUpload {
            service: self,
            temporary: temporary.clone(),
            committed: false,
        };
        self.check_transfer(control)?;
        let remote = self.transfer_argument(&temporary)?;
        let cancel = self.cancellation(control);
        self.executor
            .send(local, &remote, &cancel)
            .map_err(hdc_error)?;
        let size = self.stat_with_control(&temporary, control)?.size;
        anyhow::ensure!(size == before.len(), FileTransferEntryError::SourceChanged);
        event(HdcTransferEvent::Phase(FileTransferPhase::Committing));
        self.commit_upload(&staged.temporary, target, replace, control)?;
        staged.committed = true;
        let after = self.stat_with_control(target, control)?;
        anyhow::ensure!(
            after.size == before.len() && after.file_type == SftpFileType::File,
            FileTransferEntryError::SourceChanged
        );
        let bytes = before.len();
        event(HdcTransferEvent::Data(bytes));
        Ok(bytes)
    }

    pub(crate) fn exists_with_control(
        &self,
        path: &RemoteFilePath,
        control: &SftpTransferControl,
    ) -> anyhow::Result<bool> {
        match self.stat_with_control(path, control) {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(
                    error.downcast_ref::<nyaterm_harmony::error::HdcError>(),
                    Some(nyaterm_harmony::error::HdcError::FileMissing)
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn commit_upload(
        &self,
        temporary: &RemoteFilePath,
        target: &RemoteFilePath,
        replace: bool,
        control: &SftpTransferControl,
    ) -> anyhow::Result<()> {
        let from = quote(&raw_path(temporary)?)?;
        let to = quote(&raw_path(target)?)?;
        if replace {
            self.command(
                &format!("mv -f {from} {to} 2>/dev/null || echo {FAILED}"),
                control,
            )
        } else {
            self.rename_guarded(&from, &to, control)
        }
    }

    /// Copy a remote file on the same device without staging through the host.
    pub(crate) fn copy_remote_file(
        &self,
        source: &RemoteFilePath,
        target: &RemoteFilePath,
        replace: bool,
        control: &SftpTransferControl,
    ) -> anyhow::Result<u64> {
        self.check_transfer(control)?;
        let size = self.stat_with_control(source, control)?.size;
        let from = quote(&raw_path(source)?)?;
        let to = quote(&raw_path(target)?)?;
        if replace {
            self.command(
                &format!("cp -f {from} {to} 2>/dev/null || echo {FAILED}"),
                control,
            )?;
        } else {
            let output = self.shell(
                &format!(
                    "if [ -e {to} ] || [ -L {to} ]; then echo {exists}; \
                     else cp {from} {to} 2>/dev/null || echo {FAILED}; fi",
                    exists = crate::hdc::EXISTS,
                ),
                control,
            )?;
            let text = String::from_utf8_lossy(&output);
            if text.lines().any(|line| line.trim() == crate::hdc::EXISTS) {
                anyhow::bail!(FileTransferEntryError::TargetExists);
            }
            anyhow::ensure!(
                !text.lines().any(|line| line.trim() == FAILED),
                nyaterm_harmony::error::HdcError::Rejected
            );
        }
        let copied = self.stat_with_control(target, control)?.size;
        anyhow::ensure!(copied == size, FileTransferEntryError::SourceChanged);
        Ok(size)
    }

    fn rename_guarded(
        &self,
        from: &str,
        to: &str,
        control: &SftpTransferControl,
    ) -> anyhow::Result<()> {
        let output = self.shell(
            &format!(
                "if [ -e {to} ] || [ -L {to} ]; then echo {exists}; \
                 else mv {from} {to} 2>/dev/null || echo {FAILED}; fi",
                exists = crate::hdc::EXISTS,
            ),
            control,
        )?;
        let text = String::from_utf8_lossy(&output);
        if text.lines().any(|line| line.trim() == crate::hdc::EXISTS) {
            anyhow::bail!(FileTransferEntryError::TargetExists);
        }
        anyhow::ensure!(
            !text.lines().any(|line| line.trim() == FAILED),
            nyaterm_harmony::error::HdcError::Rejected
        );
        Ok(())
    }
}

fn same_content(before: HdcFileStat, after: HdcFileStat) -> anyhow::Result<()> {
    anyhow::ensure!(
        before.size == after.size
            && before.modified == after.modified
            && before.file_type == after.file_type,
        FileTransferEntryError::SourceChanged
    );
    Ok(())
}
