//! HarmonyOS PTY bridged through a long-lived `hdc shell` subprocess.
//!
//! Unlike the ADB backend there is no structured shell-v2 framing here: the
//! child's stdin/stdout pipes are the terminal stream, so window-size updates
//! are local bookkeeping only and the remote exit status is whatever `hdc`
//! itself reports when the shell channel closes.
use std::io::{Read, Write};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::session_event_queue::SessionEventQueue;
use crate::session_types::{SessionEvent, SessionInfo, TerminalTransport};
use nyaterm_harmony::client::{DeviceBinding, HdcClient, reap};

const INPUT_BYTES: usize = 8 * 1024 * 1024;
/// Bounds how long the reader waits for the child to report its status after
/// the output pipe closes, before falling back to a generic disconnect reason.
const EXIT_STATUS_GRACE: Duration = Duration::from_secs(5);

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
    child: Arc<std::sync::Mutex<Child>>,
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
        let client = HdcClient {
            executable: binding.executable.clone(),
        };
        let mut child = client.spawn_shell(&binding.connect_key)?;
        let stdin_pipe = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("hdc stdin unavailable"))?;
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("hdc stdout unavailable"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("hdc stderr unavailable"))?;
        let child = Arc::new(std::sync::Mutex::new(child));
        let stopped = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::sync_channel::<WriteRequest>(256);
        let writer_stop = Arc::clone(&stopped);
        let writer_queued = Arc::clone(&queued);
        let writer_events = events.clone();
        let writer_id = info.id.clone();
        let writer_worker = nyaterm_core::runtime::worker::spawn(
            format!("hdc-shell-writer-{}", info.id),
            move || {
                let mut stdin = stdin_pipe;
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
                    stdout_pipe,
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
        // stderr is folded into the output stream; its EOF alone never ends the
        // session, only the stdout reader reports the exit.
        let stderr_stop = Arc::clone(&stopped);
        let stderr_id = info.id.clone();
        let stderr_events = events.clone();
        let stderr_worker = match nyaterm_core::runtime::worker::spawn(
            format!("hdc-shell-stderr-{}", info.id),
            move || {
                drain_output(stderr_pipe, &stderr_events, &stderr_id, &stderr_stop, &None);
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
                let _ = reader_worker.join();
                return Err(error.into());
            }
        };
        Ok(Self {
            info,
            child,
            tx,
            stopped,
            queued,
            workers: vec![writer_worker, reader_worker, stderr_worker],
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

/// Pump one output pipe into terminal output. When `child` is present (the
/// stdout reader), a closed pipe ends the session and the reported reason
/// waits for the child status.
fn drain_output(
    mut pipe: impl Read + Send + 'static,
    events: &SessionEventQueue,
    session_id: &str,
    stopped: &AtomicBool,
    child: &Option<Arc<std::sync::Mutex<Child>>>,
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
            return format!("HarmonyShellExit:{}", status.code().unwrap_or(0));
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
        // The subprocess protocol carries no window-size channel; the local
        // projection is the only thing that can follow the user's pane.
        self.info.cols = cols;
        self.info.rows = rows;
        Ok(())
    }
    fn close(&mut self) -> anyhow::Result<()> {
        self.stop();
        drop(self.tx.clone());
        // Wait for the child so pipes close and readers reach EOF, bounded by
        // the shared reaping helper.
        if let Ok(mut child) = self.child.lock() {
            let _ = reap(&mut child, Duration::from_secs(2));
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
}
