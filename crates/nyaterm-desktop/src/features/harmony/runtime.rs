//! Saved-connection start for HarmonyOS devices: resolve `hdc`, probe the
//! target, then hand a fixed binding to the terminal transport.
use super::labels::HarmonyError;
use crate::features::session::SavedConnectionStartOptions;
use crate::features::{NyaTermApp, runtime_jobs::await_blocking_job};
use crate::models::SessionLaunchConfig;
use crate::models::session::HarmonySessionConfig;
use gpui::Context;
use nyaterm_core::{AiExecutionProfile, ConnectionType, SavedConnection};
use nyaterm_harmony::client::{DeviceBinding, HdcClient};
use nyaterm_harmony::error::HdcError;
use rust_i18n::t;

impl NyaTermApp {
    /// Log a stable code and surface a localized message; cancellation is quiet
    /// so abandoned start attempts do not spam the user.
    pub(super) fn report_harmony_error(
        &mut self,
        operation: &'static str,
        request_id: &str,
        error: &HarmonyError,
        cx: &mut Context<Self>,
    ) {
        let code = error.code();
        if code == "Cancelled" {
            return;
        }
        tracing::warn!(
            operation,
            backend = "Hdc",
            request_id,
            error_code = code,
            "HarmonyOS operation failed"
        );
        let message = error.message();
        self.shell.set_status(message.clone());
        self.notify_background_operation(
            "harmony-operation",
            nyaterm_ui::notification::NyaNotificationKind::Error,
            message,
            cx,
        );
    }

    pub(in crate::features) fn start_harmony_connection(
        &mut self,
        connection: SavedConnection,
        options: SavedConnectionStartOptions,
        cx: &mut Context<Self>,
    ) {
        if !self.session.start_request_can_start(&options) {
            return;
        }
        let ConnectionType::Harmony { device, .. } = connection.config.clone() else {
            return;
        };
        let key = connection.id.clone();
        if let Some(request_id) = self.harmony.pending.get(&key)
            && self
                .session
                .start_pending_entries()
                .any(|(id, _)| id == request_id)
        {
            return;
        }
        let mut options = self.prepare_session_start_options(options);
        options.request_id = Some(
            self.session
                .begin_saved_start_preparation(&connection, &options),
        );
        let attempt = self
            .session
            .start_attempt(options.request_id.as_ref().unwrap());
        self.shell.clear_last_connect_failure();
        self.shell.show_workspace();
        self.ensure_pending_session_status_clock(cx);
        self.harmony
            .pending
            .insert(key.clone(), options.request_id.clone().unwrap());
        let custom = self.settings.summary().harmony_hdc_path.clone();
        let name = connection.name.clone();
        let task_key = key.clone();
        let task = self.blocking_jobs.submit_task("harmony-bind", move |_| {
            let check_cancelled = || {
                if attempt.is_cancelled() {
                    Err(HdcError::Cancelled)
                } else {
                    Ok(())
                }
            };
            check_cancelled()?;
            let client =
                HdcClient::local((!custom.is_empty()).then(|| std::path::Path::new(&custom)))?;
            check_cancelled()?;
            let binding = DeviceBinding::from_selector(&client.executable, &device)?;
            // `hdc shell true` doubles as an online probe: unreachable keys fail
            // fast instead of surfacing as a dead terminal tab.
            client.probe(&binding.connect_key)?;
            check_cancelled()?;
            Ok::<_, HdcError>(HarmonySessionConfig {
                name,
                device_key: task_key,
                device,
                binding,
                cols: 80,
                rows: 24,
            })
        });
        self.shell
            .set_status(t!("harmony.connecting", name = connection.name).into_owned());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = await_blocking_job(task)
                .await
                .map_err(|_| HarmonyError::JobFailed)
                .and_then(|value| value.map_err(HarmonyError::Backend));
            let _ = this.update(cx, |app, cx| {
                if app.harmony.pending.get(&key) != options.request_id.as_ref() {
                    return;
                }
                app.harmony.pending.remove(&key);
                if !app.session.start_request_can_start(&options) {
                    return;
                }
                match result {
                    Ok(config) => app.open_harmony_shell(config, options, cx),
                    Err(error) => {
                        app.report_harmony_error(
                            "harmony-bind",
                            options.request_id.as_deref().unwrap_or(""),
                            &error,
                            cx,
                        );
                        app.fail_start_preparation(&options, error.message(), cx);
                        app.shell.set_status(error.message());
                    }
                }
                cx.notify();
            });
            let _ = NyaTermApp::update_in_main_window(&this, cx, |app, window, cx| {
                app.pump_startup_restore_queue_if_ready(window, cx);
            });
        })
        .detach();
    }

    fn open_harmony_shell(
        &mut self,
        config: HarmonySessionConfig,
        options: SavedConnectionStartOptions,
        cx: &mut Context<Self>,
    ) {
        let device_key = config.device_key.clone();
        // The same hdc binding serves the terminal and the file browser, so the
        // pane can open without a second probe of the device.
        self.harmony.register_files(
            &device_key,
            std::sync::Arc::new(nyaterm_files::hdc::HdcFileService::new(&config.binding)),
        );
        self.begin_background_session_start(
            t!("harmony.shellTitle", name = config.name).into_owned(),
            SessionLaunchConfig::HdcShell(config),
            Some(device_key),
            AiExecutionProfile::SendOnly,
            options,
            cx,
        );
    }
}
