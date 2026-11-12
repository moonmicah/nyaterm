//! Persisted HarmonyOS (hdc) settings and UI-independent presentation policies.

use serde::{Deserialize, Serialize};

/// Identifies a HarmonyOS/OpenHarmony device the way `hdc` does.
///
/// USB devices are addressed by the `connect-key` reported by `hdc list targets`;
/// TCP devices are addressed by `ip:port` after `hdc tconn`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HdcDeviceSelector {
    Usb { connect_key: String },
    Tcp { host: String, port: u16 },
}

impl HdcDeviceSelector {
    /// Target key passed to `hdc -t`. Mirrors the TCP bracketing of `AdbDeviceSelector::serial`.
    pub fn connect_key(&self) -> String {
        match self {
            Self::Usb { connect_key } => connect_key.clone(),
            Self::Tcp { host, port } if host.contains(':') => format!("[{host}]:{port}"),
            Self::Tcp { host, port } => format!("{host}:{port}"),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Usb { connect_key }
                if connect_key.is_empty() || connect_key.chars().any(char::is_control) =>
            {
                Err("Select an authorized HarmonyOS device")
            }
            Self::Tcp { host, port }
                if host.trim().is_empty()
                    || host.chars().any(char::is_whitespace)
                    || *port == 0 =>
            {
                Err("Enter a valid HarmonyOS host and port")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HarmonyViewKind {
    Files,
    Shell,
}

impl HarmonyViewKind {
    pub const fn restore_type(self) -> &'static str {
        match self {
            Self::Files => "hdc_files",
            Self::Shell => "hdc_shell",
        }
    }

    pub fn from_restore_type(value: &str) -> Option<Self> {
        match value {
            "hdc_files" => Some(Self::Files),
            "hdc_shell" => Some(Self::Shell),
            _ => None,
        }
    }
}

/// `/data/local/tmp/` is the only path `hdc file send` reliably accepts on every
/// OpenHarmony/HarmonyOS release; shared-storage browsing depends on device policy
/// and must be verified per device before it becomes the default.
pub fn default_harmony_directory() -> String {
    "/data/local/tmp/".into()
}

#[cfg(test)]
mod tests {
    use crate::models::harmony::{HarmonyViewKind, HdcDeviceSelector};

    #[test]
    fn device_identity_does_not_include_a_runtime_transport_id() {
        let device = HdcDeviceSelector::Tcp {
            host: "::1".into(),
            port: 10178,
        };
        assert_eq!(device.connect_key(), "[::1]:10178");
        assert_eq!(serde_json::to_value(device).unwrap()["kind"], "tcp");
        assert_eq!(
            HdcDeviceSelector::Usb {
                connect_key: "conn-key-1".into()
            }
            .connect_key(),
            "conn-key-1"
        );
    }

    #[test]
    fn view_kinds_round_trip_restore_types() {
        for view in [HarmonyViewKind::Files, HarmonyViewKind::Shell] {
            assert_eq!(
                HarmonyViewKind::from_restore_type(view.restore_type()),
                Some(view)
            );
        }
        assert_eq!(HarmonyViewKind::from_restore_type("adb_shell"), None);
    }

    #[test]
    fn selectors_reject_empty_or_control_keys_and_bad_tcp_hosts() {
        assert_eq!(
            HdcDeviceSelector::Usb {
                connect_key: String::new()
            }
            .validate(),
            Err("Select an authorized HarmonyOS device")
        );
        assert_eq!(
            HdcDeviceSelector::Usb {
                connect_key: "a\nb".into()
            }
            .validate(),
            Err("Select an authorized HarmonyOS device")
        );
        assert_eq!(
            HdcDeviceSelector::Tcp {
                host: " ".into(),
                port: 10178
            }
            .validate(),
            Err("Enter a valid HarmonyOS host and port")
        );
        assert_eq!(
            HdcDeviceSelector::Tcp {
                host: "example.test".into(),
                port: 0
            }
            .validate(),
            Err("Enter a valid HarmonyOS host and port")
        );
        assert_eq!(
            HdcDeviceSelector::Tcp {
                host: "example.test".into(),
                port: 10178
            }
            .validate(),
            Ok(())
        );
    }
}
