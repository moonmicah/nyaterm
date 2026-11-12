//! Presentation-only localization for hdc backend errors, mirroring
//! `android::labels`: codes are stable, messages never expose device output.
use nyaterm_harmony::error::HdcError;
use rust_i18n::t;

pub(in crate::features) enum HarmonyError {
    Backend(HdcError),
    JobFailed,
}

impl HarmonyError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::Backend(error) => match error {
                HdcError::FileMissing => "FileMissing",
                HdcError::Io(_) => "Io",
                HdcError::ExecutableMissing => "ExecutableMissing",
                HdcError::Offline => "Offline",
                HdcError::Rejected => "Rejected",
                HdcError::Protocol(_) => "Protocol",
                HdcError::Cancelled => "Cancelled",
                HdcError::Timeout => "Timeout",
                HdcError::CommandFailed(_) => "CommandFailed",
            },
            Self::JobFailed => "JobFailed",
        }
    }
    pub(super) fn message(&self) -> String {
        match self {
            Self::Backend(error) => hdc_error_message(error),
            Self::JobFailed => t!("harmony.errors.operationFailed").into_owned(),
        }
    }
}

pub(in crate::features) fn hdc_error_message(error: &HdcError) -> String {
    match error {
        HdcError::FileMissing => t!("harmony.errors.fileMissing"),
        HdcError::Io(_) => t!("harmony.errors.io"),
        HdcError::ExecutableMissing => t!("harmony.errors.executableMissing"),
        HdcError::Offline => t!("harmony.errors.offline"),
        HdcError::Rejected => t!("harmony.errors.rejected"),
        HdcError::Protocol(_) => t!("harmony.errors.protocol"),
        HdcError::Cancelled => t!("harmony.errors.cancelled"),
        HdcError::Timeout => t!("harmony.errors.timeout"),
        HdcError::CommandFailed(code) => t!("harmony.errors.commandFailed", code = code),
    }
    .into_owned()
}

/// Map transport exit/error codes; unknown future codes stay readable instead of
/// leaking raw backend text.
pub(in crate::features) fn shell_event_message(code: &str) -> String {
    match code {
        "HarmonyShellInputFailed" => t!("harmony.errors.shellInputFailed").into_owned(),
        "HarmonyDeviceDisconnected" => t!("harmony.errors.disconnected").into_owned(),
        _ => code
            .strip_prefix("HarmonyShellExit:")
            .and_then(|code| code.parse::<i32>().ok())
            .map(|code| t!("harmony.errors.shellExit", code = code).into_owned())
            .unwrap_or_else(|| t!("harmony.errors.operationFailed").into_owned()),
    }
}

pub(in crate::features) fn session_start_error_message(
    error: nyaterm_transport::SessionError,
) -> String {
    anyhow::Error::new(error)
        .chain()
        .find_map(|cause| cause.downcast_ref::<HdcError>())
        .map(hdc_error_message)
        .unwrap_or_else(|| t!("harmony.errors.operationFailed").into_owned())
}

#[cfg(test)]
mod tests {
    use super::{hdc_error_message, shell_event_message};
    use nyaterm_harmony::error::HdcError;
    use rust_i18n::t;

    #[test]
    fn hdc_errors_translate_without_exposing_backend_details() {
        assert!(
            !hdc_error_message(&HdcError::Protocol("sensitive backend context"))
                .contains("sensitive")
        );
        for code in [
            "HarmonyShellInputFailed",
            "HarmonyDeviceDisconnected",
            "unknown-future-code",
        ] {
            let message = shell_event_message(code);
            assert!(!message.is_empty());
            assert_ne!(message, code);
            assert!(!message.starts_with("harmony."));
        }
        assert!(shell_event_message("HarmonyShellExit:42").contains("42"));
    }

    #[test]
    fn harmony_messages_exist_in_all_six_locales() {
        for locale in ["zh-CN", "zh-TW", "en", "ja", "ko", "fr"] {
            for key in [
                "harmony.connecting",
                "harmony.shellTitle",
                "harmony.errors.fileMissing",
                "harmony.errors.io",
                "harmony.errors.executableMissing",
                "harmony.errors.offline",
                "harmony.errors.rejected",
                "harmony.errors.protocol",
                "harmony.errors.cancelled",
                "harmony.errors.timeout",
                "harmony.errors.commandFailed",
                "harmony.errors.operationFailed",
                "harmony.errors.shellInputFailed",
                "harmony.errors.disconnected",
                "harmony.errors.shellExit",
            ] {
                let message = t!(key, locale = locale);
                assert!(!message.is_empty(), "{locale} missing {key}");
                assert_ne!(message, key, "{locale} missing {key}");
            }
        }
    }
}
