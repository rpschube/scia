//! WSL (Windows Subsystem for Linux) detection.
//!
//! A Linux binary running inside WSL cannot see Windows system audio: WSLg's
//! PulseAudio carries only the audio of WSL applications, not the Windows mix.
//! The binary therefore detects when it is running under WSL so it can label a
//! live-capture attempt honestly and point at the two supported paths, rather
//! than presenting a black screen that only ever reacts to WSL-app sounds.
//!
//! Detection is split into a pure classifier ([`is_wsl`]) — decided entirely
//! from its inputs, so every branch is unit-tested without touching the host —
//! and a thin runtime wrapper ([`detect_wsl`]) that gathers those inputs from
//! `/proc/version` and the environment.

/// Whether the two detection inputs indicate a WSL environment.
///
/// A WSL kernel identifies itself in `/proc/version`: WSL2 reports a
/// `microsoft-standard` kernel and WSL1 a `Microsoft` build, so a
/// case-insensitive match on `"microsoft"` covers both. Independently, every WSL
/// distribution shell sets `WSL_DISTRO_NAME`; a set, non-empty value is taken as
/// WSL even on the rare kernel that does not carry the marker.
///
/// Pure: the result is a function of the arguments only, so the classifier is
/// tested directly for each case (marker present, env var set, neither).
#[must_use]
pub fn is_wsl(proc_version: &str, wsl_distro_name: Option<&str>) -> bool {
    proc_version.to_ascii_lowercase().contains("microsoft")
        || wsl_distro_name.is_some_and(|name| !name.is_empty())
}

/// Detect whether this process is running inside WSL, gathering the inputs
/// [`is_wsl`] classifies from the host: the contents of `/proc/version` (empty
/// when it cannot be read, e.g. off Linux) and the `WSL_DISTRO_NAME` environment
/// variable.
#[must_use]
pub fn detect_wsl() -> bool {
    let proc_version = std::fs::read_to_string("/proc/version").unwrap_or_default();
    let distro = std::env::var("WSL_DISTRO_NAME").ok();
    is_wsl(&proc_version, distro.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_version_microsoft_marker_is_wsl_either_case() {
        // WSL2 kernel string (lowercase `microsoft`).
        assert!(is_wsl(
            "Linux version 5.15.0-microsoft-standard-WSL2 (gcc 11.2.0)",
            None,
        ));
        // WSL1 kernel string (capitalised `Microsoft`); matched case-insensitively.
        assert!(is_wsl("Linux version 4.4.0-19041-Microsoft", None));
    }

    #[test]
    fn env_var_marks_wsl_even_without_the_kernel_marker() {
        // A bare, non-WSL kernel string, but the distro env var is set.
        assert!(is_wsl(
            "Linux version 6.1.0-23-amd64 (gcc 12.2.0)",
            Some("Ubuntu"),
        ));
    }

    #[test]
    fn neither_input_is_not_wsl() {
        assert!(!is_wsl("Linux version 6.1.0-23-amd64 (gcc 12.2.0)", None));
        // An empty env value is not a WSL marker.
        assert!(!is_wsl("Linux version 6.1.0-23-amd64", Some("")));
        // A completely empty proc-version (e.g. off Linux) with no env var.
        assert!(!is_wsl("", None));
    }
}
