//! The tests drive the same command protocol as production against an in-memory
//! device: the fake parses the shell words the service emits, so quoting, the
//! sentinel guards and the `ls -l` parser are all exercised together.

use super::listing::{ParsedEntry, parse_line};
use super::{
    EXISTS, FAILED, HdcError, HdcFileExecutor, HdcFileService, HdcFileValidationError,
    HdcShellOutput, MISSING, quote, raw_path,
};
use crate::sftp::{RemoteFilePath, SftpTransferControl};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
struct Node {
    directory: bool,
    data: Vec<u8>,
    symlink: Option<String>,
}

impl Node {
    fn directory() -> Self {
        Self {
            directory: true,
            data: Vec::new(),
            symlink: None,
        }
    }

    fn file(data: &[u8]) -> Self {
        Self {
            directory: false,
            data: data.to_vec(),
            symlink: None,
        }
    }

    fn link(target: &str) -> Self {
        Self {
            directory: false,
            data: Vec::new(),
            symlink: Some(target.to_string()),
        }
    }

    fn render(&self, name: &str) -> String {
        let file_type = if self.symlink.is_some() {
            'l'
        } else if self.directory {
            'd'
        } else {
            '-'
        };
        let size = if self.symlink.is_some() {
            self.symlink.as_deref().unwrap_or_default().len()
        } else {
            self.data.len()
        };
        let target = self
            .symlink
            .as_ref()
            .map(|target| format!(" -> {target}"))
            .unwrap_or_default();
        format!("{file_type}rwxr-xr-x 1 root root {size} 2024-05-06 07:08 {name}{target}")
    }
}

#[derive(Debug, Default)]
struct Device {
    nodes: Mutex<BTreeMap<String, Node>>,
    commands: Mutex<Vec<String>>,
    fail_send: Mutex<bool>,
}

impl Device {
    fn insert(&self, path: &str, node: Node) {
        let mut nodes = self.nodes.lock().unwrap();
        let path = normalize(path);
        let mut parent = parent_of(&path);
        loop {
            if parent.is_empty() || nodes.contains_key(&parent) {
                break;
            }
            nodes.insert(parent.clone(), Node::directory());
            if parent == "/" {
                break;
            }
            parent = parent_of(&parent);
        }
        nodes.insert(path, node);
        if !nodes.contains_key("/") {
            nodes.insert("/".into(), Node::directory());
        }
    }

    fn read(&self, path: &str) -> Option<Node> {
        self.nodes.lock().unwrap().get(&normalize(path)).cloned()
    }

    fn names(&self, path: &str) -> Vec<(String, Node)> {
        // `ls -l <link>` resolves the link on the device before listing.
        let mut path = normalize(path);
        if let Some(target) = self.read(&path).and_then(|node| node.symlink) {
            path = normalize(&target);
        }
        self.nodes
            .lock()
            .unwrap()
            .iter()
            .filter(|(candidate, _)| parent_of(candidate) == path && candidate.as_str() != path)
            .map(|(candidate, node)| (name_of(candidate).to_string(), node.clone()))
            .collect()
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

fn normalize(path: &str) -> String {
    if path.len() > 1 {
        path.trim_end_matches('/').to_string()
    } else {
        path.to_string()
    }
}

fn parent_of(path: &str) -> String {
    let path = normalize(path);
    match path.rfind('/') {
        Some(0) => "/".into(),
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

fn name_of(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    path.rsplit('/').next().unwrap_or(path)
}

/// Split a command into words, honoring the single-quote escaping that
/// [`super::quote`] produces (`'a'\''b'`).
fn words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '\'' {
                quoted = false;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' => quoted = true,
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[derive(Clone, Debug)]
struct FakeExecutor(Arc<Device>);

impl HdcFileExecutor for FakeExecutor {
    fn shell(
        &self,
        command: &str,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<HdcShellOutput, HdcError> {
        self.0.commands.lock().unwrap().push(command.to_string());
        let words = words(command);
        let stdout = self.0.dispatch(&words);
        Ok(HdcShellOutput {
            stdout,
            stderr: Vec::new(),
            status: Some(0),
        })
    }

    fn send(
        &self,
        local: &Path,
        remote: &str,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HdcError> {
        if *self.0.fail_send.lock().unwrap() {
            return Err(HdcError::CommandFailed(1));
        }
        let data = std::fs::read(local)?;
        self.0.insert(remote, Node::file(&data));
        Ok(())
    }

    fn recv(
        &self,
        remote: &str,
        local: &Path,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<(), HdcError> {
        let Some(node) = self.0.read(remote) else {
            return Err(HdcError::FileMissing);
        };
        std::fs::write(local, node.data)?;
        Ok(())
    }
}

impl Device {
    fn dispatch(&self, words: &[String]) -> Vec<u8> {
        match words.first().map(String::as_str) {
            Some("ls") => {
                let flags = words.get(1).cloned().unwrap_or_default();
                let path = first_path(words);
                let Some(mut node) = self.read(&path) else {
                    return format!("{MISSING}\n").into_bytes();
                };
                // `-d` reports the entry itself; otherwise the device follows a
                // directory link before listing, and `L` forces that for `-d` too.
                if (!flags.contains('d') || flags.contains('L'))
                    && let Some(target) = node.symlink.clone()
                {
                    node = self.read(&target).unwrap_or(node);
                }
                if flags.contains('d') {
                    return format!("{}\n", node.render(name_of(&path))).into_bytes();
                }
                if node.directory {
                    let mut output = String::new();
                    for (name, child) in self.names(&path) {
                        output.push_str(&child.render(&name));
                        output.push('\n');
                    }
                    return output.into_bytes();
                }
                format!("{}\n", node.render(name_of(&path))).into_bytes()
            }
            Some("mkdir") => {
                self.insert(&first_path(words), Node::directory());
                Vec::new()
            }
            Some("rm") => {
                let path = first_path(words);
                let mut nodes = self.nodes.lock().unwrap();
                nodes.remove(&normalize(&path));
                Vec::new()
            }
            Some("mv") | Some("cp") => {
                let paths: Vec<&String> = words
                    .iter()
                    .skip(1)
                    .filter(|word| word.starts_with('/'))
                    .collect();
                let (Some(from), Some(to)) = (paths.first(), paths.get(1)) else {
                    return format!("{FAILED}\n").into_bytes();
                };
                let Some(node) = self.read(from) else {
                    return format!("{FAILED}\n").into_bytes();
                };
                if self.read(to).is_some() && words.iter().any(|word| word == "-n") {
                    return format!("{EXISTS}\n").into_bytes();
                }
                self.insert(to, node);
                if words[0] == "mv" {
                    self.nodes.lock().unwrap().remove(&normalize(from));
                }
                Vec::new()
            }
            Some("if") => {
                // `if [ -e <to> ] || [ -L <to> ]; then echo <exists>; else mv ... fi`
                let to = first_path(words);
                if self.read(&to).is_some() {
                    return format!("{EXISTS}\n").into_bytes();
                }
                let rest: Vec<String> = words
                    .iter()
                    .skip_while(|word| *word != "else")
                    .cloned()
                    .collect();
                self.dispatch(&rest[1..])
            }
            Some("[") => {
                // `[ -e <path> ] || : > <path> 2>/dev/null || echo <failed>`
                let path = first_path(words);
                if self.read(&path).is_some() {
                    return Vec::new();
                }
                if words.iter().any(|word| word == ":") {
                    self.insert(&path, Node::file(b""));
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

fn first_path(words: &[String]) -> String {
    words
        .iter()
        .find(|word| word.starts_with('/'))
        .cloned()
        .unwrap_or_default()
}

fn service(device: &Arc<Device>) -> HdcFileService {
    HdcFileService::with_executor(Arc::new(FakeExecutor(device.clone())), "fixture".into())
}

fn device_with_tree() -> Arc<Device> {
    let device = Arc::new(Device::default());
    device.insert("/storage", Node::directory());
    device.insert("/storage/docs", Node::directory());
    device.insert("/storage/docs/a.txt", Node::file(b"hello"));
    device.insert("/storage/docs/b.txt", Node::file(b"world!"));
    device.insert("/storage/link", Node::link("/storage/docs"));
    device
}

#[test]
fn listing_reports_types_sizes_and_paths() {
    let device = device_with_tree();
    let service = service(&device);
    let entries = service
        .list_dir_path(&RemoteFilePath::new("/storage/docs"))
        .unwrap();
    assert_eq!(entries.len(), 2);
    let mut names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["a.txt", "b.txt"]);
    let file = entries
        .iter()
        .find(|entry| entry.name == "a.txt")
        .expect("a.txt");
    assert_eq!(file.file_type, crate::sftp::SftpFileType::File);
    assert_eq!(file.size, Some(5));
    assert_eq!(file.path, "/storage/docs/a.txt");
    assert_eq!(file.modified_at, Some(1_714_979_280));
    assert!(file.permissions.is_some_and(|mode| mode & 0o400 != 0));
}

#[test]
fn symlinked_directories_resolve_before_listing() {
    let device = device_with_tree();
    let service = service(&device);
    let stat = service.stat(&RemoteFilePath::new("/storage/link")).unwrap();
    assert!(stat.is_directory());
    let entries = service
        .list_dir_path(&RemoteFilePath::new("/storage/link"))
        .unwrap();
    assert_eq!(entries.len(), 2);
}

#[test]
fn missing_paths_are_stable_errors_and_exists_reports_false() {
    let device = device_with_tree();
    let service = service(&device);
    let missing = RemoteFilePath::new("/storage/nope");
    assert!(matches!(
        service
            .stat(&missing)
            .unwrap_err()
            .downcast_ref::<HdcError>(),
        Some(HdcError::FileMissing)
    ));
    assert!(!service.exists(&missing).unwrap());
    assert!(
        service
            .exists(&RemoteFilePath::new("/storage/docs/a.txt"))
            .unwrap()
    );
}

#[test]
fn create_rename_and_delete_use_the_guarded_commands() {
    let device = device_with_tree();
    let service = service(&device);
    let directory = RemoteFilePath::new("/storage/docs/new");
    service.create_dir_remote_path(&directory, None).unwrap();
    assert!(device.read("/storage/docs/new").unwrap().directory);

    let file = RemoteFilePath::new("/storage/docs/new/file");
    service.create_file_remote_path(&file, None).unwrap();
    assert!(device.read("/storage/docs/new/file").is_some());

    let renamed = RemoteFilePath::new("/storage/docs/new/renamed");
    service.rename_remote_paths(&file, &renamed).unwrap();
    assert!(device.read("/storage/docs/new/file").is_none());
    assert!(device.read("/storage/docs/new/renamed").is_some());

    service.delete_remote_path(&renamed).unwrap();
    assert!(device.read("/storage/docs/new/renamed").is_none());

    let used = device.commands();
    assert!(used.iter().any(|command| command.starts_with("mkdir -p ")));
    assert!(used.iter().any(|command| command.starts_with("rm -rf ")));
}

#[test]
fn rename_without_replacement_refuses_an_existing_destination() {
    let device = device_with_tree();
    let service = service(&device);
    let from = RemoteFilePath::new("/storage/docs/a.txt");
    let to = RemoteFilePath::new("/storage/docs/b.txt");
    let error = service
        .rename_remote_paths_no_replace(&from, &to)
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<crate::file_browser::FileTransferEntryError>(),
        Some(crate::file_browser::FileTransferEntryError::TargetExists)
    ));
    // Neither file moved.
    assert!(device.read("/storage/docs/a.txt").is_some());
}

#[test]
fn deleting_the_root_is_refused_before_any_command_runs() {
    let device = device_with_tree();
    let service = service(&device);
    assert!(
        service
            .delete_remote_path(&RemoteFilePath::new("/"))
            .is_err()
    );
    assert!(device.commands().is_empty());
}

#[test]
fn uploads_stage_then_commit_and_clean_up_after_a_failure() {
    let device = device_with_tree();
    let service = service(&device);
    let temporary = tempfile::tempdir().unwrap();
    let local = temporary.path().join("upload.txt");
    std::fs::write(&local, b"payload").unwrap();
    let target = RemoteFilePath::new("/storage/docs/upload.txt");
    let control = SftpTransferControl::new();
    let bytes = service
        .upload(&local, &target, false, &control, |_| {})
        .unwrap();
    assert_eq!(bytes, 7);
    assert_eq!(
        device.read("/storage/docs/upload.txt").unwrap().data,
        b"payload"
    );
    assert!(
        device
            .commands()
            .iter()
            .any(|command| command.contains(".nyaterm-"))
    );

    *device.fail_send.lock().unwrap() = true;
    let failed = RemoteFilePath::new("/storage/docs/second.txt");
    assert!(
        service
            .upload(&local, &failed, false, &control, |_| {})
            .is_err()
    );
    assert!(device.read("/storage/docs/second.txt").is_none());
    // The staged part file was removed again.
    let leftovers: Vec<_> = device
        .names("/storage/docs")
        .into_iter()
        .filter(|(name, _)| name.contains(".nyaterm-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn downloads_write_through_a_temporary_file_in_the_destination_directory() {
    let device = device_with_tree();
    let service = service(&device);
    let directory = tempfile::tempdir().unwrap();
    let local = directory.path().join("a.txt");
    let control = SftpTransferControl::new();
    let bytes = service
        .download(
            &RemoteFilePath::new("/storage/docs/a.txt"),
            &local,
            false,
            &control,
            |_| {},
        )
        .unwrap();
    assert_eq!(bytes, 5);
    assert_eq!(std::fs::read(&local).unwrap(), b"hello");
}

#[test]
fn text_documents_round_trip_and_detect_stale_revisions() {
    let device = device_with_tree();
    let service = service(&device);
    let path = RemoteFilePath::new("/storage/docs/a.txt");
    let document = service.read_text_document_path(&path, 1024).unwrap();
    assert_eq!(document.content, "hello");

    let saved = service
        .write_text_document_path(&path, "updated", Some(&document.revision), false)
        .unwrap();
    assert!(matches!(
        saved,
        crate::remote_file::RemoteTextWriteResult::Saved { .. }
    ));
    assert_eq!(device.read("/storage/docs/a.txt").unwrap().data, b"updated");

    let conflict = service
        .write_text_document_path(&path, "stale", Some(&document.revision), false)
        .unwrap();
    assert_eq!(
        conflict,
        crate::remote_file::RemoteTextWriteResult::Conflict
    );
    assert_eq!(device.read("/storage/docs/a.txt").unwrap().data, b"updated");

    assert!(
        service
            .write_text_document_path(&path, "forced", None, false)
            .is_err()
    );
}

#[test]
fn previews_respect_the_size_limit_before_transferring() {
    let device = device_with_tree();
    let service = service(&device);
    let path = RemoteFilePath::new("/storage/docs/b.txt");
    assert!(service.read_file_bytes_path(&path, 2).is_err());
    assert_eq!(
        service
            .read_file_bytes_path(&path, 64)
            .unwrap()
            .content_bytes,
        b"world!"
    );
}

#[test]
fn cancelled_services_never_reach_the_device() {
    let device = device_with_tree();
    let service = service(&device);
    service.cancel();
    assert!(!service.is_valid());
    assert!(
        service
            .list_dir_path(&RemoteFilePath::new("/storage/docs"))
            .is_err()
    );
    assert!(device.commands().is_empty());
}

#[test]
fn paths_must_be_absolute_and_never_contain_nul() {
    for path in ["relative", "", "/storage/\0secret"] {
        let error = raw_path(&RemoteFilePath::new(path)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HdcFileValidationError>(),
            Some(HdcFileValidationError::InvalidPath)
        ));
        assert!(!error.to_string().contains("secret"));
    }
    let service = service(&Arc::new(Device::default()));
    for name in ["", ".", "..", "a/b", "a\0b"] {
        let error = service
            .child_path(&RemoteFilePath::new("/storage/"), name)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HdcFileValidationError>(),
            Some(HdcFileValidationError::InvalidFilename)
        ));
    }
}

#[test]
fn quotes_survive_shell_escaping() {
    let raw = b"/storage/it's a name";
    let quoted = quote(raw).unwrap();
    assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
    assert_eq!(words(&quoted), vec!["/storage/it's a name".to_string()]);
}

#[test]
fn listing_lines_round_trip_through_the_fake_renderer() {
    let parsed: ParsedEntry = parse_line(&Node::file(b"abc").render("name with spaces")).unwrap();
    assert_eq!(parsed.name, "name with spaces");
    assert_eq!(parsed.size, Some(3));
}

#[test]
fn directory_uploads_and_downloads_recurse_through_the_device() {
    let device = device_with_tree();
    let service = Arc::new(service(&device));
    let local = tempfile::tempdir().unwrap();
    let source = local.path().join("bundle");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("one.txt"), b"one").unwrap();
    std::fs::create_dir(source.join("nested")).unwrap();
    std::fs::write(source.join("nested/two.txt"), b"two!").unwrap();

    let target = RemoteFilePath::new("/storage/docs/bundle");
    let summary = crate::remote_file::FileCopyRequest {
        source: crate::remote_file::FileTransferEndpoint::Local(source.clone()),
        destination: crate::remote_file::FileTransferEndpoint::Hdc {
            service: service.clone(),
            path: target.clone(),
        },
        control: SftpTransferControl::new(),
        options: crate::sftp_transfer_types::SftpPathTransferOptions::default(),
    }
    .execute_with_progress(|_| {})
    .unwrap();
    assert!(summary.copied);
    assert_eq!(summary.bytes, 7);
    assert_eq!(
        device.read("/storage/docs/bundle/one.txt").unwrap().data,
        b"one"
    );
    assert_eq!(
        device
            .read("/storage/docs/bundle/nested/two.txt")
            .unwrap()
            .data,
        b"two!"
    );

    let restored = local.path().join("restored");
    let summary = crate::remote_file::FileCopyRequest {
        source: crate::remote_file::FileTransferEndpoint::Hdc {
            service: service.clone(),
            path: target,
        },
        destination: crate::remote_file::FileTransferEndpoint::Local(restored.clone()),
        control: SftpTransferControl::new(),
        options: crate::sftp_transfer_types::SftpPathTransferOptions::default(),
    }
    .execute_with_progress(|_| {})
    .unwrap();
    assert!(summary.copied);
    assert_eq!(
        std::fs::read(restored.join("nested/two.txt")).unwrap(),
        b"two!"
    );
}

#[test]
fn conflicting_destinations_follow_the_duplicate_policy() {
    let device = device_with_tree();
    let service = Arc::new(service(&device));
    let local = tempfile::tempdir().unwrap();
    let source = local.path().join("a.txt");
    std::fs::write(&source, b"replacement").unwrap();
    let target = RemoteFilePath::new("/storage/docs/a.txt");
    let request = |policy| crate::remote_file::FileCopyRequest {
        source: crate::remote_file::FileTransferEndpoint::Local(source.clone()),
        destination: crate::remote_file::FileTransferEndpoint::Hdc {
            service: service.clone(),
            path: target.clone(),
        },
        control: SftpTransferControl::new(),
        options: crate::sftp_transfer_types::SftpPathTransferOptions::new(
            policy,
            None,
            crate::sftp_transfer_types::SftpTransferOptions::default(),
        ),
    };

    let skipped = request(crate::sftp_transfer_types::SftpDuplicatePolicy::Skip)
        .execute_with_progress(|_| {})
        .unwrap();
    assert!(!skipped.copied);
    assert_eq!(device.read("/storage/docs/a.txt").unwrap().data, b"hello");

    // `Ask` without a resolver must fail rather than silently overwrite.
    assert!(
        request(crate::sftp_transfer_types::SftpDuplicatePolicy::Ask)
            .execute_with_progress(|_| {})
            .is_err()
    );

    let overwritten = request(crate::sftp_transfer_types::SftpDuplicatePolicy::Overwrite)
        .execute_with_progress(|_| {})
        .unwrap();
    assert!(overwritten.copied);
    assert_eq!(
        device.read("/storage/docs/a.txt").unwrap().data,
        b"replacement"
    );
}
