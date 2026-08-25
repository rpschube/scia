//! **Audio-source attribution**: name the application that is actually feeding
//! the output mix, even when it publishes no media session.
//!
//! The metadata backends ([`crate::smtc`] / [`crate::mpris`]) can only report an
//! app that speaks a media-session protocol (SMTC / MPRIS). A game — the most
//! common "what am I looking at?" case — publishes *no* media session at all, so
//! those backends see nothing. The OS output mixer, however, knows every process
//! producing audio and its live level (this is what the Windows Volume Mixer
//! shows). This module is a **sibling of the metadata backends**: it observes
//! that mixer on its own low-priority thread and reports the dominant
//! audio-producing app by name.
//!
//! # The seam
//!
//! [`start`] mirrors the metadata-backend contract exactly: it takes a
//! [`Sender<SourceEvent>`], runs one dedicated thread, and returns a
//! [`MetaHandle`] whose drop stops and joins that thread. The thread polls the
//! mixer at a lazy [`POLL_INTERVAL`] (cheap; the OS query is a snapshot of the
//! current sessions), reduces each audio session to a [`SourceSample`], and runs
//! them through a [`DominantSelector`] whose choice is stabilised with brief
//! hysteresis so two comparably-loud apps cannot flap the label. Only a change
//! in the winning app is emitted:
//!
//! - [`SourceEvent::Dominant`] — a new app now dominates the mix; carries its
//!   friendly name.
//! - [`SourceEvent::Cleared`] — nothing audible is producing sound. A **normal**
//!   state (silence, or no mixer), never an error.
//!
//! # Platforms
//!
//! - **Windows** (real): `IAudioSessionManager2::GetSessionEnumerator` on the
//!   default render endpoint yields every audio session; each session's
//!   `IAudioSessionControl2` gives the owning PID and `IAudioMeterInformation`
//!   gives the peak level. The PID is mapped to its process image name and
//!   friendlified ([`friendly_name`]). Compiled by the Windows build only; its
//!   runtime behaviour is verified on a real Windows machine, not here.
//! - **Every other platform** (honest stub): the same thread runs but observes
//!   no sources, so the observer stays quiet. Reading the PipeWire graph on Linux
//!   would require linking `libpipewire` dev libraries that are not present on
//!   the standard build host (the existing `capture-pipewire` feature is off by
//!   default for exactly this reason), so Linux is left as a documented stub
//!   behind this identical seam rather than a dependency the gate cannot build.
//!
//! The selection policy, hysteresis, and name friendlifying are
//! platform-neutral pure code, unit-tested on every OS.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use crate::MetaHandle;

/// How often the observer samples the mixer. Deliberately lazy: attribution is
/// an ambient label, not a per-frame signal, and the OS session query should
/// stay cheap.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The longest the observer thread sleeps between stop-flag checks, so a dropped
/// [`MetaHandle`] stops it well within one [`POLL_INTERVAL`].
const POLL_CAP: Duration = Duration::from_millis(250);

/// Peak level below which a session is treated as silent. Linear amplitude in
/// `0.0..=1.0`; low enough to catch soft audio, high enough to ignore silence and
/// denormal noise so a paused-but-present session never wins the label.
const AUDIBLE_FLOOR: f32 = 0.005;

/// How many consecutive polls a *different* app must be the loudest before it
/// takes the label from the incumbent. At [`POLL_INTERVAL`] this is the brief
/// hysteresis that stops two comparably-loud apps from flapping the label; a
/// first winner (no incumbent) is adopted immediately.
const HYSTERESIS_TICKS: u32 = 2;

/// An event from the audio-source observer, mirroring the metadata backends'
/// event contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEvent {
    /// A new application now dominates the output mix; carries its friendly name
    /// (see [`friendly_name`]).
    Dominant(String),
    /// Nothing audible is producing sound. A normal idle state, never an error.
    Cleared,
}

/// One audio session's contribution to the mix at a single poll.
///
/// `name` is already the friendly, display-ready app label; `peak` is the
/// session's recent peak meter level in `0.0..=1.0`.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceSample {
    /// The friendly, display-ready application name.
    pub name: String,
    /// Recent peak meter level, linear amplitude in `0.0..=1.0`.
    pub peak: f32,
}

impl SourceSample {
    /// Convenience constructor.
    #[must_use]
    pub fn new(name: impl Into<String>, peak: f32) -> Self {
        Self {
            name: name.into(),
            peak,
        }
    }
}

/// The dominant-source policy with hysteresis.
///
/// Each poll, [`update`](DominantSelector::update) is handed the tick's
/// [`SourceSample`]s and returns the app that should own the label. The loudest
/// audible sample is the candidate; the rule is:
///
/// - No audible sample → no dominant (returns `None`), and any incumbent is
///   dropped.
/// - No incumbent → the loudest audible app is adopted immediately.
/// - The incumbent is still the loudest → it keeps the label (any pending
///   challenger is forgotten).
/// - A *different* app is the loudest → it must stay the loudest for
///   [`HYSTERESIS_TICKS`] consecutive polls before it takes over, so a brief
///   overtake does not flap the label.
///
/// The policy is pure and platform-neutral; the observer thread owns one
/// instance across polls.
#[derive(Debug, Default)]
pub struct DominantSelector {
    /// The app currently holding the label, if any.
    current: Option<String>,
    /// A rival that has been the loudest for a run of polls: its name and the
    /// consecutive-poll count. Cleared whenever the incumbent reasserts.
    challenger: Option<(String, u32)>,
}

impl DominantSelector {
    /// A fresh selector with no incumbent.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The app currently holding the label, if any.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Fold one poll's samples into the policy and return the dominant app.
    pub fn update(&mut self, samples: &[SourceSample]) -> Option<&str> {
        // The loudest audible sample is the only candidate this tick.
        let loudest = samples
            .iter()
            .filter(|s| s.peak >= AUDIBLE_FLOOR)
            .max_by(|a, b| a.peak.total_cmp(&b.peak));

        let Some(loudest) = loudest else {
            // Silence: drop the incumbent and any challenger.
            self.current = None;
            self.challenger = None;
            return None;
        };

        match &self.current {
            // First winner: adopt at once, no hysteresis to earn.
            None => {
                self.current = Some(loudest.name.clone());
                self.challenger = None;
            }
            // The incumbent is still loudest: it holds, challenger resets.
            Some(cur) if *cur == loudest.name => {
                self.challenger = None;
            }
            // A different app leads: it must sustain the lead to take over.
            Some(_) => {
                let count = match &mut self.challenger {
                    Some((name, c)) if *name == loudest.name => {
                        *c += 1;
                        *c
                    }
                    _ => {
                        self.challenger = Some((loudest.name.clone(), 1));
                        1
                    }
                };
                if count >= HYSTERESIS_TICKS {
                    self.current = Some(loudest.name.clone());
                    self.challenger = None;
                }
            }
        }

        self.current.as_deref()
    }
}

/// Turn a raw process image name into a friendly, display-ready app label.
///
/// Strips a trailing `.exe` (case-insensitive), splits on separators
/// (`-`, `_`, `.`, space) and `camelCase`/`PascalCase` boundaries, then
/// title-cases each word — preserving all-caps acronyms (`VLC`, `OBS`) as-is. So
/// `"chrome.exe"` → `"Chrome"`, `"HellLetLoose.exe"` → `"Hell Let Loose"`,
/// `"vlc.exe"` (`"VLC"` when the image is upper-cased) is kept, and a name with
/// no reconstructable spacing (`"hll-win64-shipping.exe"`) becomes the honest
/// `"Hll Win64 Shipping"`. Pure; unit-tested on every OS.
#[must_use]
pub fn friendly_name(process: &str) -> String {
    let stem = strip_exe(process);

    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower_or_digit = false;
    for ch in stem.chars() {
        if matches!(ch, ' ' | '_' | '-' | '.') {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower_or_digit = false;
            continue;
        }
        // camelCase boundary: an uppercase letter right after a lowercase letter
        // or digit starts a new word (HellLet -> Hell, Let).
        if ch.is_ascii_uppercase() && prev_lower_or_digit && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
        prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
    }
    if !cur.is_empty() {
        words.push(cur);
    }

    words
        .iter()
        .map(|w| title_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip a trailing `.exe`/`.EXE` (case-insensitive) from a process image name.
fn strip_exe(process: &str) -> &str {
    if process.len() >= 4 && process[process.len() - 4..].eq_ignore_ascii_case(".exe") {
        &process[..process.len() - 4]
    } else {
        process
    }
}

/// Title-case one word, preserving a multi-letter all-caps acronym unchanged.
fn title_word(w: &str) -> String {
    let is_acronym = w.len() >= 2
        && w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && w.chars().any(|c| c.is_ascii_uppercase());
    if is_acronym {
        return w.to_string();
    }
    let mut chars = w.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
        }
    }
}

/// Start the audio-source observer on its own low-priority thread and return a
/// [`MetaHandle`] that stops and joins it on drop. Events are pushed to `out`.
///
/// Never blocks and never fails: on a platform (or machine) with no observable
/// mixer the thread simply reports nothing and idles until stopped.
#[must_use]
pub fn start(out: Sender<SourceEvent>) -> MetaHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = thread::Builder::new()
        .name("scia-source".into())
        .spawn(move || run(&out, &thread_stop))
        .expect("spawn scia-source thread");
    // The loop polls the stop flag every POLL_CAP, so a dropped handle stops it
    // promptly without a wake trigger — no waker needed (as with the SMTC backend).
    MetaHandle::new(stop, Vec::new(), vec![join])
}

/// The observer thread body: initialise the platform (COM on Windows), then poll
/// the mixer at [`POLL_INTERVAL`], emitting only when the dominant app changes.
fn run(out: &Sender<SourceEvent>, stop: &AtomicBool) {
    // Held for the whole thread: on Windows this owns the COM apartment, torn
    // down on return; elsewhere it is a no-op guard.
    let _com = platform::ComGuard::init();

    let mut selector = DominantSelector::new();
    let mut last: Option<String> = None;

    while !stop.load(Ordering::Relaxed) {
        let samples = platform::collect();
        let current = selector.update(&samples).map(str::to_owned);
        if current != last {
            let event = match &current {
                Some(name) => SourceEvent::Dominant(name.clone()),
                None => SourceEvent::Cleared,
            };
            let _ = out.send(event);
            last = current;
        }

        // Sleep one poll interval in small chunks so the stop flag is observed
        // within POLL_CAP of a drop rather than after a full interval.
        let mut slept = Duration::ZERO;
        while slept < POLL_INTERVAL {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let chunk = POLL_CAP.min(POLL_INTERVAL - slept);
            thread::sleep(chunk);
            slept += chunk;
        }
    }
}

/// The Windows audio-session observer. Every COM object is created, used, and
/// released on the single `scia-source` thread; nothing COM crosses a thread
/// boundary. Compiled only on Windows and verified on a real machine — never
/// exercised by this crate's off-Windows tests.
#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
    use windows::Win32::Media::Audio::{
        IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, MMDeviceEnumerator,
        eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::{Interface, PWSTR};

    use super::{SourceSample, friendly_name};

    /// Owns the thread's COM apartment for the observer's lifetime.
    pub struct ComGuard {
        /// Whether this guard actually initialised COM (and so must uninitialise
        /// it). A benign `RPC_E_CHANGED_MODE`/`S_FALSE` leaves this `false`.
        owned: bool,
    }

    impl ComGuard {
        pub fn init() -> Self {
            // SAFETY: CoInitializeEx/CoUninitialize are balanced on this one
            // thread; every COM object created here stays on it.
            unsafe {
                let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
                ComGuard { owned: hr.is_ok() }
            }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.owned {
                // SAFETY: pairs with the CoInitializeEx in `init` on this thread.
                unsafe { CoUninitialize() }
            }
        }
    }

    /// Enumerate the default render endpoint's audio sessions and reduce each to a
    /// [`SourceSample`]. Any COM failure (no endpoint, no session manager) is the
    /// normal "nothing observable" state: it yields an empty vector, never a
    /// panic.
    #[must_use]
    pub fn collect() -> Vec<SourceSample> {
        // SAFETY: runs on the COM-initialised observer thread (see `ComGuard`);
        // every COM object below is created, used, and released here.
        unsafe { collect_inner().unwrap_or_default() }
    }

    /// The fallible body: a COM error short-circuits to `None`, which `collect`
    /// treats as "no sources this tick".
    unsafe fn collect_inner() -> Option<Vec<SourceSample>> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).ok()?;
            let sessions = manager.GetSessionEnumerator().ok()?;
            let count = sessions.GetCount().unwrap_or(0);

            let mut out = Vec::new();
            for i in 0..count {
                let Ok(control) = sessions.GetSession(i) else {
                    continue;
                };
                // The owning process id; 0 is the system-sounds session (no real
                // app), which the mixer shows separately — skip it.
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let pid = control2.GetProcessId().unwrap_or(0);
                if pid == 0 {
                    continue;
                }
                let Ok(meter) = control.cast::<IAudioMeterInformation>() else {
                    continue;
                };
                let peak = meter.GetPeakValue().unwrap_or(0.0);
                let Some(image) = process_image_basename(pid) else {
                    continue;
                };
                out.push(SourceSample::new(friendly_name(&image), peak));
            }
            Some(out)
        }
    }

    /// The bare image file name (e.g. `chrome.exe`) of a process, or `None` if it
    /// cannot be opened or queried (a more-privileged process, or one that exited
    /// mid-poll).
    unsafe fn process_image_basename(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 260]; // MAX_PATH
            let mut size = buf.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            let full = String::from_utf16_lossy(&buf[..size as usize]);
            let base = full.rsplit(['\\', '/']).next().unwrap_or(&full);
            Some(base.to_string())
        }
    }
}

/// The non-Windows observer: an honest stub behind the same seam. The thread runs
/// and joins cleanly (so the lifecycle is identical everywhere), but observes no
/// sources — reading the PipeWire graph would need a `libpipewire` dependency the
/// standard build host cannot compile (see the module docs).
#[cfg(not(windows))]
mod platform {
    use super::SourceSample;

    /// A no-op COM guard on platforms without COM.
    pub struct ComGuard;

    impl ComGuard {
        pub fn init() -> Self {
            ComGuard
        }
    }

    /// No observable sources off Windows.
    #[must_use]
    pub fn collect() -> Vec<SourceSample> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, peak: f32) -> SourceSample {
        SourceSample::new(name, peak)
    }

    // ---- friendly_name ----------------------------------------------------

    #[test]
    fn friendly_strips_exe_and_capitalises() {
        assert_eq!(friendly_name("chrome.exe"), "Chrome");
        assert_eq!(friendly_name("Chrome.EXE"), "Chrome");
        assert_eq!(friendly_name("firefox"), "Firefox");
    }

    #[test]
    fn friendly_splits_camel_case() {
        assert_eq!(friendly_name("HellLetLoose.exe"), "Hell Let Loose");
        assert_eq!(friendly_name("PascalCase"), "Pascal Case");
    }

    #[test]
    fn friendly_splits_separators() {
        assert_eq!(
            friendly_name("hll-win64-shipping.exe"),
            "Hll Win64 Shipping"
        );
        assert_eq!(friendly_name("my_cool_app.exe"), "My Cool App");
    }

    #[test]
    fn friendly_preserves_acronyms() {
        assert_eq!(friendly_name("VLC.exe"), "VLC");
        assert_eq!(friendly_name("OBS.exe"), "OBS");
    }

    #[test]
    fn friendly_handles_empty() {
        assert_eq!(friendly_name(""), "");
        assert_eq!(friendly_name(".exe"), "");
    }

    // ---- DominantSelector: dominant selection -----------------------------

    #[test]
    fn no_audible_source_yields_none() {
        let mut sel = DominantSelector::new();
        // Present but below the audible floor: silence, no winner.
        assert_eq!(
            sel.update(&[sample("Chrome", 0.0), sample("Spotify", 0.001)]),
            None
        );
        assert_eq!(sel.update(&[]), None);
    }

    #[test]
    fn loudest_audible_app_wins() {
        let mut sel = DominantSelector::new();
        assert_eq!(
            sel.update(&[sample("Chrome", 0.2), sample("Game", 0.8)]),
            Some("Game")
        );
    }

    #[test]
    fn first_winner_is_adopted_immediately() {
        // With no incumbent there is no hysteresis to earn.
        let mut sel = DominantSelector::new();
        assert_eq!(sel.update(&[sample("Game", 0.9)]), Some("Game"));
    }

    #[test]
    fn incumbent_holds_while_still_loudest() {
        let mut sel = DominantSelector::new();
        assert_eq!(sel.update(&[sample("Game", 0.9)]), Some("Game"));
        assert_eq!(
            sel.update(&[sample("Game", 0.7), sample("Chrome", 0.6)]),
            Some("Game")
        );
    }

    // ---- DominantSelector: hysteresis -------------------------------------

    #[test]
    fn a_brief_overtake_does_not_flap_the_label() {
        let mut sel = DominantSelector::new();
        assert_eq!(sel.update(&[sample("Game", 0.9)]), Some("Game"));
        // Chrome edges ahead for a single tick: not enough to take over.
        assert_eq!(
            sel.update(&[sample("Game", 0.5), sample("Chrome", 0.6)]),
            Some("Game"),
            "one louder tick must not switch the label"
        );
        // Game reasserts: the challenger is forgotten.
        assert_eq!(
            sel.update(&[sample("Game", 0.9), sample("Chrome", 0.1)]),
            Some("Game")
        );
    }

    #[test]
    fn a_sustained_overtake_switches_after_hysteresis() {
        let mut sel = DominantSelector::new();
        assert_eq!(sel.update(&[sample("Game", 0.9)]), Some("Game"));
        // Chrome leads for HYSTERESIS_TICKS consecutive polls, then takes over.
        assert_eq!(
            sel.update(&[sample("Game", 0.4), sample("Chrome", 0.8)]),
            Some("Game"),
            "first louder tick: still the incumbent"
        );
        assert_eq!(
            sel.update(&[sample("Game", 0.4), sample("Chrome", 0.8)]),
            Some("Chrome"),
            "sustained lead takes the label after hysteresis"
        );
    }

    #[test]
    fn silence_clears_the_incumbent() {
        let mut sel = DominantSelector::new();
        assert_eq!(sel.update(&[sample("Game", 0.9)]), Some("Game"));
        assert_eq!(sel.update(&[]), None, "silence drops the incumbent");
        // A later app is adopted fresh (immediately, no incumbent).
        assert_eq!(sel.update(&[sample("Spotify", 0.5)]), Some("Spotify"));
    }

    #[test]
    fn interrupted_challenger_resets() {
        let mut sel = DominantSelector::new();
        assert_eq!(sel.update(&[sample("Game", 0.9)]), Some("Game"));
        // Chrome leads once...
        assert_eq!(
            sel.update(&[sample("Game", 0.4), sample("Chrome", 0.8)]),
            Some("Game")
        );
        // ...then a third app leads once (challenger switches, count resets)...
        assert_eq!(
            sel.update(&[sample("Game", 0.4), sample("Discord", 0.8)]),
            Some("Game")
        );
        // ...so Chrome leading once more is still only its first sustained tick.
        assert_eq!(
            sel.update(&[sample("Game", 0.4), sample("Chrome", 0.8)]),
            Some("Game"),
            "a switched challenger restarts the hysteresis count"
        );
    }
}
