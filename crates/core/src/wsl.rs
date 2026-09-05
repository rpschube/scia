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
pub fn is_wsl(windows_build: bool, proc_version: &str, wsl_distro_name: Option<&str>) -> bool {
    // A native Windows build is never "inside WSL" — it is the escape *from*
    // it, with working WASAPI capture. WSL interop injects `WSL_DISTRO_NAME`
    // into Windows children launched from a WSL shell (even when the shell
    // unsets it), so without this gate the Windows binary run via interop
    // misclassifies itself and opens the WSL guidance screen on every launch.
    if windows_build {
        return false;
    }
    proc_version.to_ascii_lowercase().contains("microsoft")
        || wsl_distro_name.is_some_and(|name| !name.is_empty())
}

/// Detect whether this process is running inside WSL, gathering the inputs
/// [`is_wsl`] classifies from the host: the contents of `/proc/version` (empty
/// when it cannot be read, e.g. off Linux) and the `WSL_DISTRO_NAME` environment
/// variable.
///
/// A native Windows build is never "inside WSL" — it is the escape *from* it,
/// with working WASAPI capture. WSL interop injects `WSL_DISTRO_NAME` into
/// Windows children launched from a WSL shell (even when the shell unsets it),
/// so without the compile-time gate the Windows binary run via interop would
/// misclassify itself and open the WSL guidance screen on every launch.
#[must_use]
pub fn detect_wsl() -> bool {
    let proc_version = std::fs::read_to_string("/proc/version").unwrap_or_default();
    let distro = std::env::var("WSL_DISTRO_NAME").ok();
    is_wsl(cfg!(windows), &proc_version, distro.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_version_microsoft_marker_is_wsl_either_case() {
        // WSL2 kernel string (lowercase `microsoft`).
        assert!(is_wsl(
            false,
            "Linux version 5.15.0-microsoft-standard-WSL2 (gcc 11.2.0)",
            None,
        ));
        // WSL1 kernel string (capitalised `Microsoft`); matched case-insensitively.
        assert!(is_wsl(false, "Linux version 4.4.0-19041-Microsoft", None));
    }

    #[test]
    fn env_var_marks_wsl_even_without_the_kernel_marker() {
        // A bare, non-WSL kernel string, but the distro env var is set.
        assert!(is_wsl(
            false,
            "Linux version 6.1.0-23-amd64 (gcc 12.2.0)",
            Some("Ubuntu"),
        ));
    }

    #[test]
    fn neither_input_is_not_wsl() {
        assert!(!is_wsl(
            false,
            "Linux version 6.1.0-23-amd64 (gcc 12.2.0)",
            None
        ));
        // An empty env value is not a WSL marker.
        assert!(!is_wsl(false, "Linux version 6.1.0-23-amd64", Some("")));
        // A completely empty proc-version (e.g. off Linux) with no env var.
        assert!(!is_wsl(false, "", None));
    }
}

#[cfg(test)]
mod windows_gate_tests {
    use super::*;

    #[test]
    fn windows_build_is_never_wsl_even_with_interop_markers() {
        // WSL interop injects WSL_DISTRO_NAME into Windows children; the
        // build-flavor gate must win over every environment marker.
        assert!(!is_wsl(
            true,
            "Linux version 5.15.0-microsoft-standard-WSL2",
            Some("Ubuntu")
        ));
        assert!(!is_wsl(true, "", Some("Ubuntu")));
    }
}
