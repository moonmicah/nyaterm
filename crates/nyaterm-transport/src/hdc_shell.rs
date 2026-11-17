//! HarmonyOS PTY bridged through a long-lived `hdc shell` subprocess.
//!
//! Unlike the ADB backend there is no structured shell-v2 framing here: the
//! child's terminal stream is whatever the PTY master yields, and the remote
//! exit status is whatever `hdc` itself reports when the shell channel closes.
//!
//! The child always runs under a real pseudo-terminal. `hdc shell` reads its
//! input from the attached console only — with piped stdio it prints
//! "Not support stdio TTY mode" and never forwards keystrokes (upstream
//! declined to support redirected input, developtools_hdc issue I5ECHL), so a
//! PTY is the only interactive surface it honors.
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::session_event_queue::SessionEventQueue;
use crate::session_types::{SessionEvent, SessionInfo, TerminalTransport};
use nyaterm_harmony::client::DeviceBinding;
use nyaterm_harmony::error::HdcError;

const INPUT_BYTES: usize = 8 * 1024 * 1024;
/// Bounds how long the reader waits for the child to report its status after
/// the output stream closes, before falling back to a generic disconnect reason.
const EXIT_STATUS_GRACE: Duration = Duration::from_secs(5);
/// hdc prints this diagnostic on stdout when its stdio is not a terminal. The
/// bridge always runs the child under a PTY, so the notice is not expected;
/// the filter stays as a safety net for older or unusual hdc builds. It
/// describes the transport, not the session, so it is dropped instead of
/// surfacing in the terminal.
const TTY_MODE_NOTICE: &str = "Not support stdio TTY mode";

/// Drops complete lines that contain the known one-shot hdc diagnostic while
/// passing everything else through byte-for-byte.
struct WithoutHdcNotices<R: Read> {
    inner: R,
    pending: Vec<u8>,
}

fn contains_sub(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

impl<R: Read> WithoutHdcNotices<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pending: Vec::new(),
        }
    }

    /// Appends `data`, filtering every complete line. An unterminated tail is
    /// buffered so a notice split across reads is still recognised.
    fn absorb(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let Some(last_newline) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
            return;
        };
        let complete: Vec<u8> = self.pending.drain(..=last_newline).collect();
        let mut kept = Vec::with_capacity(complete.len());
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            if !contains_sub(line, TTY_MODE_NOTICE.as_bytes()) {
                kept.extend_from_slice(line);
            }
        }
        // Re-insert the kept lines ahead of the buffered tail.
        let mut filtered = kept;
        filtered.extend_from_slice(&self.pending);
        self.pending = filtered;
    }
}

impl<R: Read> Read for WithoutHdcNotices<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            let mut chunk = [0u8; 8192];
            let count = self.inner.read(&mut chunk)?;
            if count == 0 {
                // EOF: any unterminated tail goes out verbatim unless it is
                // the notice itself.
                if contains_sub(&self.pending, TTY_MODE_NOTICE.as_bytes()) {
                    self.pending.clear();
                }
                return Ok(0);
            }
            self.absorb(&chunk[..count]);
        }
        let count = out.len().min(self.pending.len());
        let rest = self.pending.split_off(count);
        out[..count].copy_from_slice(&self.pending);
        self.pending = rest;
        Ok(count)
    }
}

#[derive(Clone, Debug)]
pub struct HdcShellConfig {
    pub name: String,
    pub binding: DeviceBinding,
    pub cols: u16,
    pub rows: u16,
}

struct WriteRequest {
    data: Vec<u8>,
}

pub(crate) struct HdcTerminalTransport {
    pub(crate) info: SessionInfo,
    child: Arc<std::sync::Mutex<Box<dyn Child + Send + Sync>>>,
    master: std::sync::Mutex<Box<dyn MasterPty + Send>>,
    tx: mpsc::SyncSender<WriteRequest>,
    stopped: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
    workers: Vec<JoinHandle<()>>,
}

impl HdcTerminalTransport {
    pub(crate) fn open(
        info: SessionInfo,
        binding: &DeviceBinding,
        events: SessionEventQueue,
        mut check: impl FnMut() -> nyaterm_harmony::error::HdcResult<()>,
    ) -> anyhow::Result<Self> {
        check()?;
        // The PTY is what makes the session interactive at all: hdc forwards
        // keystrokes only when its stdio is a console, never through pipes.
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: info.rows,
                cols: info.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| HdcError::Io(std::io::Error::other(error)))?;
        let mut command = CommandBuilder::new(&binding.executable);
        command.arg("-t");
        command.arg(&binding.connect_key);
        command.arg("shell");
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| HdcError::Io(std::io::Error::other(error)))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| HdcError::Io(std::io::Error::other(error)))?;
        let reader = WithoutHdcNotices::new(reader);
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| HdcError::Io(std::io::Error::other(error)))?;
        let child: Arc<std::sync::Mutex<Box<dyn Child + Send + Sync>>> =
            Arc::new(std::sync::Mutex::new(child));
        let master = std::sync::Mutex::new(pair.master);
        let stopped = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::sync_channel::<WriteRequest>(256);
        let writer_stop = Arc::clone(&stopped);
        let writer_queued = Arc::clone(&queued);
        let writer_events = events.clone();
        let writer_id = info.id.clone();
        let stdin = writer;
        let writer_worker = nyaterm_core::runtime::worker::spawn(
            format!("hdc-shell-writer-{}", info.id),
            move || {
                let mut stdin = stdin;
                while !writer_stop.load(Ordering::Acquire) {
                    let request = match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(request) => request,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    };
                    writer_queued.fetch_sub(request.data.len(), Ordering::AcqRel);
                    let result = stdin.write_all(&request.data).and_then(|()| stdin.flush());
                    if result.is_err() {
                        writer_events.push(SessionEvent::Error {
                            session_id: writer_id.clone(),
                            message: "HarmonyShellInputFailed".into(),
                        });
                        writer_stop.store(true, Ordering::Release);
                        break;
                    }
                }
            },
        )?;
        let reader_stop = Arc::clone(&stopped);
        let reader_id = info.id.clone();
        let reader_child = Arc::clone(&child);
        let reader_events = events.clone();
        let reader_worker = match nyaterm_core::runtime::worker::spawn(
            format!("hdc-shell-reader-{}", info.id),
            move || {
                let reason = drain_output(
                    reader,
                    &reader_events,
                    &reader_id,
                    &reader_stop,
                    &Some(Arc::clone(&reader_child)),
                );
                reader_stop.store(true, Ordering::Release);
                reader_events.push(SessionEvent::Exited {
                    session_id: reader_id,
                    reason,
                });
            },
        ) {
            Ok(worker) => worker,
            Err(error) => {
                stopped.store(true, Ordering::Release);
                if let Ok(mut child) = child.lock() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let _ = writer_worker.join();
                return Err(error.into());
            }
        };
        Ok(Self {
            info,
            child,
            master,
            tx,
            stopped,
            queued,
            workers: vec![writer_worker, reader_worker],
        })
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }

    fn enqueue(&self, data: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.stopped.load(Ordering::Acquire),
            "Harmony shell is closed"
        );
        self.queued
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used + data.len() <= INPUT_BYTES).then_some(used + data.len())
            })
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::WouldBlock))?;
        if let Err(error) = self.tx.try_send(WriteRequest {
            data: data.to_vec(),
        }) {
            self.queued.fetch_sub(data.len(), Ordering::AcqRel);
            return Err(std::io::Error::from(match error {
                mpsc::TrySendError::Full(_) => std::io::ErrorKind::WouldBlock,
                mpsc::TrySendError::Disconnected(_) => std::io::ErrorKind::BrokenPipe,
            })
            .into());
        }
        Ok(())
    }
}

/// Pump one output stream into terminal output. When `child` is present (the
/// main reader), a closed stream ends the session and the reported reason
/// waits for the child status.
fn drain_output(
    mut pipe: impl Read + Send + 'static,
    events: &SessionEventQueue,
    session_id: &str,
    stopped: &AtomicBool,
    child: &Option<Arc<std::sync::Mutex<Box<dyn Child + Send + Sync>>>>,
) -> String {
    let mut bytes = [0; 8192];
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }
        match pipe.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => events.push(SessionEvent::Output {
                session_id: session_id.to_owned(),
                data: bytes[..count].to_vec(),
            }),
            Err(_) => break,
        }
    }
    let Some(child) = child else {
        return "HarmonyDeviceDisconnected".to_owned();
    };
    let deadline = Instant::now() + EXIT_STATUS_GRACE;
    loop {
        if let Ok(mut child) = child.lock()
            && let Ok(Some(status)) = child.try_wait()
        {
            return format!("HarmonyShellExit:{}", status.exit_code());
        }
        if stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
            return "HarmonyDeviceDisconnected".to_owned();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl TerminalTransport for HdcTerminalTransport {
    fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.enqueue(data)
    }
    fn resize(&mut self, cols: u16, rows: u16, _width: u16, _height: u16) -> anyhow::Result<()> {
        self.info.cols = cols;
        self.info.rows = rows;
        // The child runs under a pseudo-terminal, so the device-side shell can
        // follow the user's pane; failures only cost cursor-aware programs.
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        Ok(())
    }
    fn close(&mut self) -> anyhow::Result<()> {
        self.stop();
        drop(self.tx.clone());
        // The stop() kill closes the pseudo-terminal, readers reach EOF, and
        // the child reaps promptly; bound the wait anyway.
        if let Ok(mut child) = self.child.lock() {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    _ if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    _ => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        Ok(())
    }
}

impl Drop for HdcTerminalTransport {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::WithoutHdcNotices;
    use crate::SessionManager;
    use crate::hdc_shell::HdcShellConfig;
    use nyaterm_harmony::client::DeviceBinding;
    use nyaterm_harmony::error::HdcError;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn cancelled_hdc_handshake_does_not_register_or_leak_a_session() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let manager = SessionManager::new();
        let started = Instant::now();
        let result = manager.create_hdc_session_with_cancel(
            HdcShellConfig {
                name: "fixture".into(),
                binding: DeviceBinding {
                    executable: PathBuf::from("hdc-fixture"),
                    connect_key: "fixture".into(),
                },
                cols: 80,
                rows: 24,
            },
            || {
                if cancel.load(Ordering::Acquire) {
                    Err(HdcError::Cancelled)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(
            result.err(),
            Some(crate::SessionError::Hdc(error)) if error.is::<HdcError>()
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(manager.list_sessions().unwrap().is_empty());
        // The check closure runs before any spawn, so a flip after the attempt
        // still must not leave anything behind on a second call.
        cancelled.store(true, Ordering::Release);
        assert!(
            manager
                .create_hdc_session_with_cancel(
                    HdcShellConfig {
                        name: "fixture".into(),
                        binding: DeviceBinding {
                            executable: PathBuf::from("hdc-fixture"),
                            connect_key: "fixture".into(),
                        },
                        cols: 80,
                        rows: 24,
                    },
                    || {
                        if cancelled.load(Ordering::Acquire) {
                            Err(HdcError::Cancelled)
                        } else {
                            Ok(())
                        }
                    },
                )
                .is_err()
        );
        assert!(manager.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn a_missing_executable_fails_before_registering_a_session() {
        let manager = SessionManager::new();
        let result = manager.create_hdc_session(HdcShellConfig {
            name: "fixture".into(),
            binding: DeviceBinding {
                executable: PathBuf::from("definitely-missing-hdc-fixture"),
                connect_key: "fixture".into(),
            },
            cols: 80,
            rows: 24,
        });
        assert!(result.is_err());
        assert!(manager.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn the_tty_mode_notice_is_dropped_and_other_output_survives() {
        use std::io::Read;

        let input = b"[W][2026-11-17 08:18:51] Not support stdio TTY mode\r\n$ ls\r\ntotal 0\r\n";
        let mut filtered = WithoutHdcNotices::new(&input[..]);
        let mut output = Vec::new();
        filtered.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"$ ls\r\ntotal 0\r\n");
    }

    #[test]
    fn a_notice_split_across_reads_is_still_filtered() {
        use std::io::Read;

        // One byte per read: the notice line is only complete once every
        // fragment has been buffered, so partial matches must never leak.
        let input = b"hello\r\n[W][x] Not support stdio TTY mode\r\nbye";
        for step in 1..input.len() {
            let mut reader = WithoutHdcNotices::new(&input[..]);
            let mut chunk = Vec::new();
            {
                let mut limited = (&mut reader).take(step as u64);
                limited.read_to_end(&mut chunk).unwrap();
            }
            let mut rest = Vec::new();
            reader.read_to_end(&mut rest).unwrap();
            chunk.extend_from_slice(&rest);
            assert_eq!(chunk, b"hello\r\nbye", "read step {step}");
        }
    }
}
