//! Recursive copies whose source or destination is a HarmonyOS device.
//!
//! Mirrors `crate::adb::copy` for the hdc backend: the destination endpoint is
//! already the final path, conflict policy comes from the transfer options, and
//! device-to-device copies stage through a local temporary file because hdc only
//! moves bytes between the host and the device.

use crate::file_browser::{
    FileBrowserService, FileTransferEntryError, valid_file_browser_child_name,
};
use crate::hdc::HdcFilePolicyError;
use crate::remote_file::copy_progress::CopyProgress;
use crate::remote_file::{FileCopyRequest, FileCopySummary, FileTransferEndpoint};
use crate::sftp::{RemoteFilePath, SftpFileType};
use crate::sftp_transfer_types::{
    SftpDuplicateCacheKey, SftpDuplicateDecision, SftpDuplicatePolicy, SftpDuplicateRequest,
    SftpTransferDirection, SftpTransferProgress,
};

const MAX_DEPTH: usize = 64;

fn browser(endpoint: &FileTransferEndpoint) -> (FileBrowserService, RemoteFilePath) {
    match endpoint {
        FileTransferEndpoint::Local(path) => (
            FileBrowserService::local(),
            RemoteFilePath::new(path.to_string_lossy()),
        ),
        FileTransferEndpoint::Hdc { service, path } => {
            (FileBrowserService::Hdc(service.clone()), path.clone())
        }
        FileTransferEndpoint::Adb { service, path } => {
            (FileBrowserService::Adb(service.clone()), path.clone())
        }
        FileTransferEndpoint::Remote { service, path } => {
            (FileBrowserService::Remote(service.clone()), path.clone())
        }
    }
}

fn exists(endpoint: &FileTransferEndpoint) -> anyhow::Result<bool> {
    match endpoint {
        FileTransferEndpoint::Hdc { service, path } => service.exists(path),
        FileTransferEndpoint::Adb { service, path } => service.exists(path),
        FileTransferEndpoint::Local(path) => match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        },
        FileTransferEndpoint::Remote { service, path } => Ok(service
            .list_dir_path(&path.parent()?)?
            .into_iter()
            .any(|entry| entry.path == path.display_path)),
    }
}

fn wait_request(request: &FileCopyRequest) -> anyhow::Result<()> {
    loop {
        request.control.check_cancelled()?;
        for endpoint in [&request.source, &request.destination] {
            if let FileTransferEndpoint::Hdc { service, .. } = endpoint {
                service.check()?;
            }
        }
        if !request.control.is_paused() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub(crate) fn execute(
    request: FileCopyRequest,
    callback: impl FnMut(SftpTransferProgress) + Send + 'static,
) -> anyhow::Result<FileCopySummary> {
    // `touch` on HarmonyOS has no epoch form, so timestamp preservation is
    // reported before any bytes move instead of failing halfway through.
    anyhow::ensure!(
        !request.options.transfer_options().preserve_timestamps,
        HdcFilePolicyError::TimestampUnavailable
    );
    let staged = !matches!(request.source, FileTransferEndpoint::Local(_))
        && !matches!(request.destination, FileTransferEndpoint::Local(_));
    let progress = CopyProgress::new(
        &request.source,
        &request.destination,
        if staged { 2 } else { 1 },
        callback,
    );
    progress.scan(&request.source, &request.control)?;
    let (bytes, copied, target) = node(&request, &progress, 0)?;
    if copied {
        progress.finish(bytes);
    }
    Ok(FileCopySummary {
        bytes,
        copied,
        destination_path: target,
        used_local_staging: staged,
    })
}

fn node(
    request: &FileCopyRequest,
    progress: &CopyProgress,
    depth: usize,
) -> anyhow::Result<(u64, bool, String)> {
    anyhow::ensure!(depth <= MAX_DEPTH, FileTransferEntryError::LimitExceeded);
    wait_request(request)?;
    let (source, source_path) = browser(&request.source);
    let properties = source.remote_file_properties(&source_path)?;
    if depth == 0
        && let Some(expected) = request.options.expected_source_type()
    {
        anyhow::ensure!(
            properties.file_type == expected,
            FileTransferEntryError::SourceChanged
        );
    }
    anyhow::ensure!(
        matches!(
            properties.file_type,
            SftpFileType::File | SftpFileType::Directory
        ),
        FileTransferEntryError::UnsupportedFileType
    );
    let directory = properties.is_directory();
    let mut target_endpoint = request.destination.clone();
    let (target, mut path) = browser(&request.destination);
    if depth == 0 {
        source.validate_copy_destination(&source_path, &target, &path, directory)?;
    }
    let mut replace = false;
    if exists(&target_endpoint)? {
        match resolve_conflict(request, &source, &source_path, &target, &path, directory)? {
            Some(SftpDuplicateDecision::Skip) => return Ok((0, false, path.display_path)),
            Some(SftpDuplicateDecision::Overwrite) => replace = true,
            Some(SftpDuplicateDecision::Rename) => {
                let parent = target.parent_path(&path)?;
                let name = path
                    .display_path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("invalid destination name"))?
                    .to_string();
                let mut renamed = None;
                for number in 1..=10_000 {
                    let candidate = target.child_path(&parent, &format!("{name} ({number})"))?;
                    if !exists(&target.transfer_endpoint(&candidate))? {
                        renamed = Some(candidate);
                        break;
                    }
                }
                let Some(renamed) = renamed else {
                    anyhow::bail!("no available copy destination");
                };
                path = renamed;
                target_endpoint = target.transfer_endpoint(&path);
            }
            None => {}
        }
    }
    if directory {
        if !exists(&target_endpoint)? {
            target.create_dir_remote_path(&path, None)?;
        }
        anyhow::ensure!(
            target.remote_file_properties(&path)?.file_type == SftpFileType::Directory,
            "destination is not a directory"
        );
        let mut total = 0_u64;
        for entry in source.list_dir_path(&source_path)? {
            request.control.check_cancelled()?;
            if matches!(entry.name.as_str(), "." | "..") {
                continue;
            }
            anyhow::ensure!(
                valid_file_browser_child_name(target.kind(), &entry.name),
                "destination cannot represent filename"
            );
            let child = target.child_path(&path, &entry.name)?;
            let nested = FileCopyRequest {
                source: source.transfer_endpoint(&entry.remote_path()),
                destination: target.transfer_endpoint(&child),
                control: request.control.clone(),
                options: request
                    .options
                    .with_transfer_options(request.options.transfer_options().clone()),
            };
            total = total
                .checked_add(node(&nested, progress, depth + 1)?.0)
                .ok_or_else(|| anyhow::anyhow!("copy size overflow"))?;
        }
        progress.finish_item();
        return Ok((total, true, path.display_path));
    }
    let bytes = match (&request.source, &request.destination) {
        (FileTransferEndpoint::Local(local), FileTransferEndpoint::Hdc { service, path }) => {
            service.upload_with_options(
                local,
                path,
                replace,
                &request.control,
                request.options.transfer_options(),
                |event| match event {
                    crate::hdc::HdcTransferEvent::Data(bytes) => progress.add_bytes(bytes),
                    crate::hdc::HdcTransferEvent::Phase(phase) => progress.phase(phase),
                },
            )?
        }
        (FileTransferEndpoint::Hdc { service, path }, FileTransferEndpoint::Local(local)) => {
            service.download_with_options(
                path,
                local,
                replace,
                &request.control,
                request.options.transfer_options(),
                |event| match event {
                    crate::hdc::HdcTransferEvent::Data(bytes) => progress.add_bytes(bytes),
                    crate::hdc::HdcTransferEvent::Phase(phase) => progress.phase(phase),
                },
            )?
        }
        (
            FileTransferEndpoint::Hdc {
                service: origin,
                path: origin_path,
            },
            FileTransferEndpoint::Hdc {
                service: destination,
                path: destination_path,
            },
        ) if origin.same_device(destination) => {
            origin.copy_remote_file(origin_path, destination_path, replace, &request.control)?
        }
        _ => staged_file(request, &target_endpoint, progress)?,
    };
    progress.finish_item();
    Ok((bytes, true, path.display_path))
}

/// Route a copy that hdc cannot perform directly through a local temporary file.
fn staged_file(
    request: &FileCopyRequest,
    destination: &FileTransferEndpoint,
    progress: &CopyProgress,
) -> anyhow::Result<u64> {
    let temporary = tempfile::tempdir()?;
    let local = temporary.path().join("content");
    match &request.source {
        FileTransferEndpoint::Hdc { service, path } => {
            service.download_with_options(
                path,
                &local,
                false,
                &request.control,
                request.options.transfer_options(),
                |event| match event {
                    crate::hdc::HdcTransferEvent::Data(bytes) => progress.add_bytes(bytes),
                    crate::hdc::HdcTransferEvent::Phase(phase) => progress.phase(phase),
                },
            )?;
        }
        _ => {
            FileCopyRequest {
                source: request.source.clone(),
                destination: FileTransferEndpoint::Local(local.clone()),
                control: request.control.clone(),
                options: request.options.clone(),
            }
            .execute_with_progress(report_delta(progress))?;
        }
    }
    match destination {
        FileTransferEndpoint::Hdc { service, path } => service.upload_with_options(
            &local,
            path,
            true,
            &request.control,
            request.options.transfer_options(),
            |event| match event {
                crate::hdc::HdcTransferEvent::Data(bytes) => progress.add_bytes(bytes),
                crate::hdc::HdcTransferEvent::Phase(phase) => progress.phase(phase),
            },
        ),
        _ => Ok(FileCopyRequest {
            source: FileTransferEndpoint::Local(local),
            destination: destination.clone(),
            control: request.control.clone(),
            options: request.options.clone(),
        }
        .execute_with_progress(report_delta(progress))?
        .bytes),
    }
}

fn report_delta(progress: &CopyProgress) -> impl FnMut(SftpTransferProgress) + Send + 'static {
    let progress = progress.clone();
    let mut reported = 0_u64;
    move |value| {
        let delta = value.bytes_transferred.saturating_sub(reported);
        reported = value.bytes_transferred;
        if delta > 0 {
            progress.add_bytes(delta);
        }
    }
}

fn resolve_conflict(
    request: &FileCopyRequest,
    source: &FileBrowserService,
    source_path: &RemoteFilePath,
    target: &FileBrowserService,
    path: &RemoteFilePath,
    directory: bool,
) -> anyhow::Result<Option<SftpDuplicateDecision>> {
    let cache_key = SftpDuplicateCacheKey::Copy {
        source_kind: source.kind(),
        source_path: if source.kind() == crate::file_browser::FileBrowserBackendKind::Local {
            source_path.display_path.as_bytes().to_vec()
        } else {
            source_path
                .raw_path()?
                .unwrap_or_else(|| source_path.display_path.as_bytes().to_vec())
        },
        target_kind: target.kind(),
        target_path: if target.kind() == crate::file_browser::FileBrowserBackendKind::Local {
            path.display_path.as_bytes().to_vec()
        } else {
            path.raw_path()?
                .unwrap_or_else(|| path.display_path.as_bytes().to_vec())
        },
        is_directory: directory,
    };
    let decision = match request.options.duplicate_policy() {
        SftpDuplicatePolicy::Overwrite => SftpDuplicateDecision::Overwrite,
        SftpDuplicatePolicy::Skip => SftpDuplicateDecision::Skip,
        SftpDuplicatePolicy::Rename => SftpDuplicateDecision::Rename,
        SftpDuplicatePolicy::Ask => {
            if let Some(decision) = request
                .options
                .cached_duplicate_decision(&cache_key)
                .map_err(anyhow::Error::msg)?
            {
                decision
            } else {
                let direction = match (&request.source, &request.destination) {
                    (FileTransferEndpoint::Local(_), _) => SftpTransferDirection::Upload,
                    (_, FileTransferEndpoint::Local(_)) => SftpTransferDirection::Download,
                    _ => SftpTransferDirection::Copy,
                };
                let decision = request
                    .options
                    .duplicate_resolver()
                    .ok_or_else(|| anyhow::anyhow!("file conflict requires a decision"))?
                    .resolve_duplicate(&SftpDuplicateRequest {
                        direction,
                        source_path: source_path.display_path.clone(),
                        target_path: path.display_path.clone(),
                        is_directory: directory,
                    })
                    .map_err(anyhow::Error::msg)?;
                request
                    .options
                    .remember_duplicate_decision(cache_key.clone(), decision)
                    .map_err(anyhow::Error::msg)?;
                decision
            }
        }
    };
    Ok(Some(decision))
}
