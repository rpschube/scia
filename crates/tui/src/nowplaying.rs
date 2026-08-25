//! Now-playing metadata for the TUI: the runtime that owns the platform
//! backend and a decode worker, the [`NowPlayingState`] the render loop keeps up
//! to date from the event stream, live position extrapolation, the overlay
//! panel, and the art-palette → scene-palette bridge.
//!
//! # Runtime
//!
//! [`MetaRuntime::spawn`] starts the platform backend (MPRIS on Linux, SMTC on
//! Windows; nothing elsewhere) on an mpsc channel, plus a decode worker thread
//! that turns encoded artwork into a downscaled [`PreviewImage`] and an
//! [`ArtPalette`] off the render thread — the palette module documents that
//! extraction must never run on a frame. The render loop drains events and
//! decode results non-blockingly each tick into [`NowPlayingState`]. Dropping
//! the runtime stops the backend (its [`MetaHandle`] joins its threads) and
//! joins the worker.
//!
//! # State machine
//!
//! [`NowPlayingState::apply_event`] folds a [`MetaEvent`] into the state and, on
//! artwork for the *current* track, returns a [`DecodeJob`] to hand to the
//! worker; artwork for a stale `track_key` is ignored. [`apply_art`] stores a
//! decode result only if it still matches the current track. A `Cleared` event
//! or an absent backend leaves the state empty — a normal "nothing playing".
//!
//! [`apply_art`]: NowPlayingState::apply_art

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use scia_meta::{
    ArtPalette, MetaEvent, MetaHandle, NowPlaying, PaletteCache, PlaybackStatus, PositionInfo,
    PreviewImage, decode_preview,
};
use scia_scenes::{PALETTE_SLOTS, Palette, Rgb};

use crate::palette;

/// Longest edge, in pixels, the decode worker downscales artwork to for the
/// preview. Ample for a coarse cell mosaic; small enough to keep the decode
/// cheap and the state light.
const PREVIEW_MAX: u32 = 64;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// A decoded preview and extracted palette for one track, keyed so a late decode
/// result for a track that has since changed can be dropped.
#[derive(Clone, Debug)]
pub struct TrackArt {
    /// The [`NowPlaying::track_key`] this art belongs to.
    pub track_key: String,
    /// A downscaled RGB preview for the panel mosaic.
    pub preview: PreviewImage,
    /// The palette extracted from the same artwork.
    pub palette: ArtPalette,
}

/// The now-playing state the render loop keeps current from the event stream.
///
/// Empty by default (nothing playing). The panel and the palette-apply key read
/// it; the loop mutates it as events and decode results arrive.
#[derive(Clone, Debug, Default)]
pub struct NowPlayingState {
    /// The current track, if a player is active.
    pub current: Option<NowPlaying>,
    /// Decoded preview + palette for the current track, once it has arrived.
    pub art: Option<TrackArt>,
}

/// A unit of decode work: encoded artwork bytes for a track, to be turned into a
/// preview and palette off the render thread.
#[derive(Clone, Debug)]
pub struct DecodeJob {
    /// The track the bytes belong to.
    pub track_key: String,
    /// Raw encoded image bytes (JPEG/PNG).
    pub bytes: Vec<u8>,
}

/// A finished decode: the preview and palette for a track.
#[derive(Clone, Debug)]
pub struct ArtResult {
    /// The track the result belongs to.
    pub track_key: String,
    /// The decoded, downscaled preview.
    pub preview: PreviewImage,
    /// The extracted palette.
    pub palette: ArtPalette,
}

impl NowPlayingState {
    /// Fold one backend event into the state.
    ///
    /// Returns a [`DecodeJob`] when artwork for the *current* track arrived and
    /// must be decoded off the render thread; artwork for any other (stale)
    /// `track_key` is ignored and yields `None`. A `TrackChanged` to a new track
    /// drops the old art until the new track's art arrives; `Cleared` empties the
    /// state.
    pub fn apply_event(&mut self, ev: MetaEvent) -> Option<DecodeJob> {
        match ev {
            MetaEvent::TrackChanged(np) => {
                let same = self
                    .current
                    .as_ref()
                    .is_some_and(|c| c.track_key == np.track_key);
                if !same {
                    // The old art belongs to a different track now.
                    self.art = None;
                }
                self.current = Some(np);
                None
            }
            MetaEvent::Artwork {
                track_key, bytes, ..
            } => match &self.current {
                Some(c) if c.track_key == track_key => Some(DecodeJob { track_key, bytes }),
                // Stale: the track changed before its art arrived.
                _ => None,
            },
            MetaEvent::Cleared => {
                self.current = None;
                self.art = None;
                None
            }
        }
    }

    /// Store a decode result, but only while it still matches the current track;
    /// a result for a track that has since changed is dropped.
    pub fn apply_art(&mut self, track_key: String, preview: PreviewImage, palette: ArtPalette) {
        if self
            .current
            .as_ref()
            .is_some_and(|c| c.track_key == track_key)
        {
            self.art = Some(TrackArt {
                track_key,
                preview,
                palette,
            });
        }
    }

    /// The current track's extracted palette, when it has been decoded.
    #[must_use]
    pub fn art_palette(&self) -> Option<&ArtPalette> {
        self.art.as_ref().map(|a| &a.palette)
    }
}

/// Extrapolate the live playback offset from a position sample and the time
/// elapsed since it was reported.
///
/// While [`PlaybackStatus::Playing`] the offset advances by `elapsed`, clamped
/// to the track length when known; while paused or stopped it holds. A new
/// sample carries a fresh anchor, so extrapolation resyncs on every event
/// without extra state.
#[must_use]
pub fn extrapolated_position(
    pos: &PositionInfo,
    status: PlaybackStatus,
    elapsed: Duration,
) -> Duration {
    let mut p = pos.position;
    if status.is_playing() {
        p = p.saturating_add(elapsed);
    }
    if let Some(len) = pos.length {
        if p > len {
            p = len;
        }
    }
    p
}

/// Live progress as `(position, length)` when a bar can be drawn, extrapolating
/// against the wall clock. `None` when the player publishes no position (SMTC
/// today) or no/zero length, so the caller hides the bar.
fn live_progress(np: &NowPlaying) -> Option<(Duration, Duration)> {
    let pos = np.position.as_ref()?;
    let len = pos.length?;
    if len.is_zero() {
        return None;
    }
    let elapsed = pos.reported_at.elapsed();
    Some((extrapolated_position(pos, np.status, elapsed), len))
}

/// Convert an extracted [`ArtPalette`] into the host [`Palette`]. The slot
/// layouts mirror by design (see the palette module), so a scene re-themes
/// without change.
#[must_use]
pub fn art_palette_to_scene(art: &ArtPalette) -> Palette {
    let mut slots = [Rgb(0, 0, 0); PALETTE_SLOTS];
    for (dst, src) in slots.iter_mut().zip(art.slots.iter()) {
        *dst = Rgb(src[0], src[1], src[2]);
    }
    Palette { slots }
}

// ---------------------------------------------------------------------------
// Runtime: backend + decode worker
// ---------------------------------------------------------------------------

/// The now-playing runtime: the platform backend plus a decode worker, wired to
/// the render loop by three channels.
///
/// Construct with [`spawn`](Self::spawn); drain [`events`](Self::events) and
/// [`results`](Self::results) each tick and hand any returned [`DecodeJob`] to
/// [`submit`](Self::submit). Dropping it stops the backend and joins the worker.
pub struct MetaRuntime {
    /// The OS backend handle; `None` on a platform with no backend. Dropping it
    /// stops and joins the backend threads.
    backend: Option<MetaHandle>,
    /// Backend events (track changes, artwork, cleared).
    events: Receiver<MetaEvent>,
    /// Decode jobs to the worker. `Option` so [`Drop`] can close the channel
    /// before joining the worker.
    jobs: Option<Sender<DecodeJob>>,
    /// Finished decode results from the worker.
    results: Receiver<ArtResult>,
    /// The decode worker thread, joined on drop.
    worker: Option<JoinHandle<()>>,
}

impl MetaRuntime {
    /// Start the platform backend and the decode worker.
    ///
    /// Absence is normal: on a platform with no backend, or with no media
    /// session, no events arrive and the state stays empty.
    #[must_use]
    pub fn spawn() -> Self {
        let (event_tx, events) = mpsc::channel::<MetaEvent>();
        let backend = start_backend(event_tx);

        let (job_tx, job_rx) = mpsc::channel::<DecodeJob>();
        let (result_tx, results) = mpsc::channel::<ArtResult>();
        let worker = std::thread::Builder::new()
            .name("scia-tui-art".into())
            .spawn(move || palette_worker(&job_rx, &result_tx))
            .ok();

        Self {
            backend,
            events,
            jobs: Some(job_tx),
            results,
            worker,
        }
    }

    /// Try to receive the next backend event without blocking.
    pub fn try_event(&self) -> Option<MetaEvent> {
        self.events.try_recv().ok()
    }

    /// Try to receive the next finished decode without blocking.
    pub fn try_result(&self) -> Option<ArtResult> {
        self.results.try_recv().ok()
    }

    /// Hand a decode job to the worker (dropped silently if the worker is gone).
    pub fn submit(&self, job: DecodeJob) {
        if let Some(tx) = &self.jobs {
            let _ = tx.send(job);
        }
    }
}

impl Drop for MetaRuntime {
    fn drop(&mut self) {
        // Stop the OS backend first (this joins its own threads), then close the
        // job channel so the worker's `recv` ends, and join the worker.
        self.backend.take();
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Start the platform now-playing backend on `tx`, or `None` where there is no
/// backend for the target OS.
fn start_backend(tx: Sender<MetaEvent>) -> Option<MetaHandle> {
    #[cfg(target_os = "linux")]
    {
        Some(scia_meta::mpris::start(tx))
    }
    #[cfg(windows)]
    {
        Some(scia_meta::smtc::start(tx))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        drop(tx);
        None
    }
}

/// The decode worker: turn artwork bytes into a preview and palette, off the
/// render thread. Exits when the job channel closes.
fn palette_worker(jobs: &Receiver<DecodeJob>, results: &Sender<ArtResult>) {
    let mut cache = PaletteCache::new();
    while let Ok(DecodeJob { track_key, bytes }) = jobs.recv() {
        let Ok(preview) = decode_preview(&bytes, PREVIEW_MAX, PREVIEW_MAX) else {
            continue;
        };
        let Ok(palette) = cache.get_or_extract(&track_key, || bytes.clone()) else {
            continue;
        };
        let _ = results.send(ArtResult {
            track_key,
            preview,
            palette,
        });
    }
}

// ---------------------------------------------------------------------------
// Panel rendering
// ---------------------------------------------------------------------------

/// The narrowest body that hosts the full panel; below it (or when the body is
/// too short) the panel degrades to a single summary line, like the meter
/// bridge on a small pane.
const NP_MIN_WIDTH: u16 = 22;
/// Rows the full panel needs beyond its content: a title row and a hint row.
const NP_CHROME_ROWS: u16 = 2;
/// Preview size in cells: width, and height in half-block rows (so `2 × height`
/// source pixel rows). Clamped to whatever the panel affords.
const ART_CELLS_W: u16 = 22;
const ART_CELLS_H: u16 = 9;

/// Paint the now-playing panel over the body, like the meter bridge. The caller
/// draws it only when the toggle is on; this still guards a degenerate body and
/// degrades to one line on a small pane.
pub fn draw_now_playing(buf: &mut Buffer, body: Rect, nps: &NowPlayingState, applied: bool) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    let Some(np) = &nps.current else {
        render_np_line(buf, body, " ♪ nothing playing ");
        return;
    };

    // Rows the content wants: art + up to three text lines + progress + swatches.
    let has_art = nps.art.is_some();
    let has_swatches = nps.art_palette().is_some();
    let text_lines = 2 // title, artist
        + u16::from(np.album.is_some())
        + u16::from(live_progress(np).is_some());
    let art_h = if has_art { ART_CELLS_H } else { 0 };
    let swatch_rows = u16::from(has_swatches);
    let want_rows = NP_CHROME_ROWS + art_h + text_lines + swatch_rows;

    if body.height < want_rows.min(NP_CHROME_ROWS + 3) || body.width < NP_MIN_WIDTH {
        render_np_line(buf, body, &summary_line(np));
        return;
    }

    render_np_panel(buf, body, np, nps, applied);
}

/// One-line fallback on the body's top row.
fn render_np_line(buf: &mut Buffer, body: Rect, text: &str) {
    let style = Style::new()
        .fg(palette::OVERLAY_FG)
        .bg(palette::OVERLAY_BG)
        .add_modifier(Modifier::BOLD);
    buf.set_stringn(body.x, body.y, text, body.width as usize, style);
}

/// The compact one-line summary of the current track.
fn summary_line(np: &NowPlaying) -> String {
    let title = np.title.as_deref().unwrap_or("(unknown)");
    match np.artist.as_deref() {
        Some(artist) => format!(" ♪ {title} — {artist} "),
        None => format!(" ♪ {title} "),
    }
}

/// The full bordered panel: title, art mosaic, track text, progress, swatches.
fn render_np_panel(
    buf: &mut Buffer,
    body: Rect,
    np: &NowPlaying,
    nps: &NowPlayingState,
    applied: bool,
) {
    let fill = Style::new().bg(palette::OVERLAY_BG).fg(palette::OVERLAY_FG);

    let title = if applied {
        "now playing · palette applied"
    } else {
        "now playing"
    };

    // A clear textual status tag for a session that is not actually playing, so a
    // paused (or stopped) session in the explicit panel can never be mistaken for
    // the live audio source. Playing sessions carry no tag — the panel is
    // unchanged for them.
    let status_tag: Option<&str> = match np.status {
        PlaybackStatus::Playing => None,
        PlaybackStatus::Paused => Some("⏸ paused"),
        PlaybackStatus::Stopped => Some("■ stopped"),
    };
    // The full title row width: the title, plus the tag and a two-cell gap.
    let title_row_len = title.chars().count() + status_tag.map_or(0, |t| t.chars().count() + 2);

    // Size the panel to the widest of the art, the minimum, and the title row, so
    // neither the title nor the status tag is ever clipped; then clamp to the
    // body.
    let inner_target = ART_CELLS_W.max(NP_MIN_WIDTH - 2).max(title_row_len as u16);
    let width = (inner_target + 2).clamp(NP_MIN_WIDTH, body.width);
    let inner_w = width.saturating_sub(2) as usize;

    let has_art = nps.art.is_some();
    let has_swatches = nps.art_palette().is_some();
    let text_lines = 2 + u16::from(np.album.is_some()) + u16::from(live_progress(np).is_some());
    let art_h = if has_art { ART_CELLS_H } else { 0 };
    let swatch_rows = u16::from(has_swatches);
    let height = (NP_CHROME_ROWS + art_h + text_lines + swatch_rows).min(body.height);
    let panel = Rect::new(body.x, body.y, width, height);

    // Clear the panel background.
    for dy in 0..panel.height {
        for dx in 0..panel.width {
            if let Some(cell) = buf.cell_mut((panel.x + dx, panel.y + dy)) {
                cell.set_char(' ').set_style(fill);
            }
        }
    }

    let inner_x = panel.x + 1;
    let title_style = if applied {
        fill.add_modifier(Modifier::BOLD).fg(palette::LIVE)
    } else {
        fill.add_modifier(Modifier::BOLD)
    };
    buf.set_stringn(inner_x, panel.y, title, inner_w, title_style);
    // The status tag sits just after the title on the same row, only for a
    // non-Playing session, and only when the panel is wide enough to hold it.
    if let Some(tag) = status_tag {
        let used = title.chars().count() + 2;
        if used < inner_w {
            let tag_style = fill.add_modifier(Modifier::BOLD).fg(palette::QUIET);
            buf.set_stringn(
                inner_x + used as u16,
                panel.y,
                tag,
                inner_w - used,
                tag_style,
            );
        }
    }

    // Content cursor, kept inside the panel and clear of the hint row.
    let content_top = panel.y + 1;
    let content_bottom = panel.y + panel.height.saturating_sub(1); // hint row
    let mut y = content_top;

    // Art mosaic.
    if let Some(art) = &nps.art {
        let avail_rows = content_bottom.saturating_sub(y);
        let rows = ART_CELLS_H.min(avail_rows);
        let cols = (ART_CELLS_W as usize).min(inner_w) as u16;
        render_art(buf, inner_x, y, cols, rows, &art.preview);
        y += rows;
    }

    // Text lines.
    let put_line = |buf: &mut Buffer, text: &str, style: Style, y: &mut u16| {
        if *y < content_bottom {
            buf.set_stringn(inner_x, *y, text, inner_w, style);
            *y += 1;
        }
    };
    put_line(
        buf,
        np.title.as_deref().unwrap_or("(unknown title)"),
        fill.add_modifier(Modifier::BOLD),
        &mut y,
    );
    put_line(
        buf,
        np.artist.as_deref().unwrap_or("(unknown artist)"),
        fill,
        &mut y,
    );
    if let Some(album) = np.album.as_deref() {
        put_line(buf, album, fill.add_modifier(Modifier::DIM), &mut y);
    }
    if let Some((pos, len)) = live_progress(np) {
        let line = progress_line(pos, len, inner_w);
        put_line(buf, &line, fill, &mut y);
    }

    // Swatch row.
    if let Some(art) = nps.art_palette() {
        if y < content_bottom {
            render_swatches(buf, inner_x, y, inner_w, art, fill);
        }
    }

    // Hint row.
    let status = match np.status {
        PlaybackStatus::Playing => "▶",
        PlaybackStatus::Paused => "⏸",
        PlaybackStatus::Stopped => "■",
    };
    let hint = format!("{status}  n closes · p palette");
    buf.set_stringn(
        inner_x,
        content_bottom,
        &hint,
        inner_w,
        fill.add_modifier(Modifier::DIM),
    );
}

/// Render the preview as a half-block mosaic: each cell packs two vertically
/// adjacent source pixels — the upper as foreground of `▀`, the lower as its
/// background — reusing the same upper-half-block glyph the mosaic rasterizer
/// draws with. The preview is nearest-sampled to the `cols × rows` cell area, so
/// the panel is size-independent.
fn render_art(buf: &mut Buffer, x0: u16, y0: u16, cols: u16, rows: u16, prev: &PreviewImage) {
    if cols == 0 || rows == 0 || prev.width == 0 || prev.height == 0 || prev.pixels.is_empty() {
        return;
    }
    let src_rows = rows.saturating_mul(2);
    for cy in 0..rows {
        for cx in 0..cols {
            let top = sample(prev, cx, cy * 2, cols, src_rows);
            let bottom = sample(prev, cx, cy * 2 + 1, cols, src_rows);
            if let Some(cell) = buf.cell_mut((x0 + cx, y0 + cy)) {
                cell.set_char('▀').set_style(
                    Style::new()
                        .fg(Color::Rgb(top[0], top[1], top[2]))
                        .bg(Color::Rgb(bottom[0], bottom[1], bottom[2])),
                );
            }
        }
    }
}

/// Nearest-sample the preview at target cell `(tx, ty)` of a `tw × th` grid.
fn sample(prev: &PreviewImage, tx: u16, ty: u16, tw: u16, th: u16) -> [u8; 3] {
    let sx = (u32::from(tx) * prev.width / u32::from(tw.max(1))).min(prev.width - 1);
    let sy = (u32::from(ty) * prev.height / u32::from(th.max(1))).min(prev.height - 1);
    prev.pixels[(sy * prev.width + sx) as usize]
}

/// Format `position / length` with a proportional fill bar sized to `inner_w`.
fn progress_line(pos: Duration, len: Duration, inner_w: usize) -> String {
    let left = fmt_dur(pos);
    let right = fmt_dur(len);
    // Bar takes whatever is left after the two timestamps, brackets and spaces.
    let chrome = left.len() + right.len() + 4;
    let bar_w = inner_w.saturating_sub(chrome).max(1);
    let frac = if len.is_zero() {
        0.0
    } else {
        (pos.as_secs_f32() / len.as_secs_f32()).clamp(0.0, 1.0)
    };
    let filled = (frac * bar_w as f32).round() as usize;
    let filled = filled.min(bar_w);
    let mut bar = String::with_capacity(bar_w);
    for i in 0..bar_w {
        bar.push(if i < filled { '█' } else { '░' });
    }
    format!("{left} [{bar}] {right}")
}

/// `m:ss` for a duration.
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Render the eight palette slots as filled swatches on one row.
fn render_swatches(
    buf: &mut Buffer,
    x0: u16,
    y: u16,
    inner_w: usize,
    art: &ArtPalette,
    fill: Style,
) {
    let mut x = x0;
    let max_x = x0 + inner_w as u16;
    for slot in &art.slots {
        let style = fill.fg(Color::Rgb(slot[0], slot[1], slot[2]));
        for _ in 0..2 {
            if x >= max_x {
                return;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char('█').set_style(style);
            }
            x += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn np(title: &str, key_seed: &str, status: PlaybackStatus) -> NowPlaying {
        NowPlaying::new(
            Some(title.to_string()),
            Some(format!("artist-{key_seed}")),
            Some("album".to_string()),
            status,
            None,
            None,
        )
    }

    fn tiny_palette() -> ArtPalette {
        ArtPalette {
            dominant: [10, 20, 30],
            accents: vec![],
            light: [40, 50, 60],
            dark: [1, 2, 3],
            slots: [[0, 0, 0]; 8],
        }
    }

    fn tiny_preview() -> PreviewImage {
        PreviewImage {
            width: 2,
            height: 2,
            pixels: vec![[1, 1, 1], [2, 2, 2], [3, 3, 3], [4, 4, 4]],
        }
    }

    #[test]
    fn track_changed_sets_current_and_no_job() {
        let mut s = NowPlayingState::default();
        assert!(
            s.apply_event(MetaEvent::TrackChanged(np(
                "A",
                "a",
                PlaybackStatus::Playing
            )))
            .is_none()
        );
        assert_eq!(s.current.as_ref().unwrap().title.as_deref(), Some("A"));
    }

    #[test]
    fn artwork_for_current_track_yields_a_job() {
        let mut s = NowPlayingState::default();
        let track = np("A", "a", PlaybackStatus::Playing);
        let key = track.track_key.clone();
        s.apply_event(MetaEvent::TrackChanged(track));
        let job = s
            .apply_event(MetaEvent::Artwork {
                track_key: key.clone(),
                bytes: vec![1, 2, 3],
                source_app: None,
            })
            .expect("artwork for the current track produces a job");
        assert_eq!(job.track_key, key);
        assert_eq!(job.bytes, vec![1, 2, 3]);
    }

    #[test]
    fn artwork_for_stale_track_is_ignored() {
        let mut s = NowPlayingState::default();
        s.apply_event(MetaEvent::TrackChanged(np(
            "A",
            "a",
            PlaybackStatus::Playing,
        )));
        // Artwork tagged for a different track never matches the current one.
        let job = s.apply_event(MetaEvent::Artwork {
            track_key: "someone/else/track".to_string(),
            bytes: vec![9],
            source_app: None,
        });
        assert!(job.is_none(), "stale artwork must be ignored");
        assert!(s.art.is_none());
    }

    #[test]
    fn changing_track_drops_old_art_then_stores_matching_art() {
        let mut s = NowPlayingState::default();
        let a = np("A", "a", PlaybackStatus::Playing);
        let a_key = a.track_key.clone();
        s.apply_event(MetaEvent::TrackChanged(a));
        s.apply_art(a_key.clone(), tiny_preview(), tiny_palette());
        assert!(s.art.is_some());

        // A new track invalidates the old art.
        let b = np("B", "b", PlaybackStatus::Playing);
        let b_key = b.track_key.clone();
        s.apply_event(MetaEvent::TrackChanged(b));
        assert!(s.art.is_none(), "old art must be dropped on a track change");

        // A late result for the *old* track is dropped.
        s.apply_art(a_key, tiny_preview(), tiny_palette());
        assert!(s.art.is_none(), "art for a stale track must not be stored");

        // A result for the new track is kept.
        s.apply_art(b_key, tiny_preview(), tiny_palette());
        assert!(s.art.is_some());
    }

    #[test]
    fn cleared_empties_state() {
        let mut s = NowPlayingState::default();
        let t = np("A", "a", PlaybackStatus::Playing);
        let key = t.track_key.clone();
        s.apply_event(MetaEvent::TrackChanged(t));
        s.apply_art(key, tiny_preview(), tiny_palette());
        s.apply_event(MetaEvent::Cleared);
        assert!(s.current.is_none());
        assert!(s.art.is_none());
    }

    #[test]
    fn progress_advances_while_playing_and_clamps() {
        let pos = PositionInfo {
            position: Duration::from_secs(10),
            length: Some(Duration::from_secs(30)),
            reported_at: Instant::now(),
        };
        // Playing: advance by elapsed.
        assert_eq!(
            extrapolated_position(&pos, PlaybackStatus::Playing, Duration::from_secs(5)),
            Duration::from_secs(15)
        );
        // Clamp at length.
        assert_eq!(
            extrapolated_position(&pos, PlaybackStatus::Playing, Duration::from_secs(999)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn progress_holds_while_paused() {
        let pos = PositionInfo {
            position: Duration::from_secs(10),
            length: Some(Duration::from_secs(30)),
            reported_at: Instant::now(),
        };
        assert_eq!(
            extrapolated_position(&pos, PlaybackStatus::Paused, Duration::from_secs(5)),
            Duration::from_secs(10),
            "a paused player must not advance"
        );
    }

    #[test]
    fn progress_resyncs_on_a_new_sample() {
        // A fresh sample resets the anchor: extrapolating from it starts over.
        let first = PositionInfo {
            position: Duration::from_secs(10),
            length: Some(Duration::from_secs(200)),
            reported_at: Instant::now(),
        };
        let extrapolated =
            extrapolated_position(&first, PlaybackStatus::Playing, Duration::from_secs(50));
        assert_eq!(extrapolated, Duration::from_secs(60));
        // A new event carries position=2s; extrapolation from it is small again.
        let second = PositionInfo {
            position: Duration::from_secs(2),
            length: Some(Duration::from_secs(200)),
            reported_at: Instant::now(),
        };
        assert_eq!(
            extrapolated_position(&second, PlaybackStatus::Playing, Duration::from_secs(1)),
            Duration::from_secs(3),
            "a new sample resyncs, discarding the old drift"
        );
    }

    #[test]
    fn progress_absent_when_no_position() {
        // SMTC publishes no position today → no bar.
        let track = np("A", "a", PlaybackStatus::Playing);
        assert!(track.position.is_none());
        assert!(live_progress(&track).is_none());
    }

    #[test]
    fn progress_absent_when_no_length() {
        let mut track = np("A", "a", PlaybackStatus::Playing);
        track.position = Some(PositionInfo {
            position: Duration::from_secs(5),
            length: None,
            reported_at: Instant::now(),
        });
        assert!(live_progress(&track).is_none());
    }

    #[test]
    fn art_palette_maps_slots_in_order() {
        let mut art = tiny_palette();
        art.slots = [
            [1, 2, 3],
            [4, 5, 6],
            [7, 8, 9],
            [10, 11, 12],
            [13, 14, 15],
            [16, 17, 18],
            [19, 20, 21],
            [22, 23, 24],
        ];
        let pal = art_palette_to_scene(&art);
        for (i, slot) in art.slots.iter().enumerate() {
            assert_eq!(pal.slots[i], Rgb(slot[0], slot[1], slot[2]));
        }
    }

    /// Flatten the whole buffer into one string.
    fn buffer_text(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    /// Concatenate one buffer row into a string.
    fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn panel_tags_a_paused_session_near_the_title() {
        let nps = NowPlayingState {
            current: Some(np("Song", "s", PlaybackStatus::Paused)),
            ..Default::default()
        };
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        draw_now_playing(&mut buf, area, &nps, false);

        // The status tag reads clearly as a word, on the title row next to it.
        let title_row = row_text(&buf, 0, 40);
        assert!(
            title_row.contains("now playing"),
            "title present: {title_row:?}"
        );
        assert!(
            title_row.contains("paused"),
            "a paused session is tagged near the title: {title_row:?}"
        );
    }

    #[test]
    fn panel_tags_a_stopped_session_near_the_title() {
        let nps = NowPlayingState {
            current: Some(np("Song", "s", PlaybackStatus::Stopped)),
            ..Default::default()
        };
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        draw_now_playing(&mut buf, area, &nps, false);
        let title_row = row_text(&buf, 0, 40);
        assert!(
            title_row.contains("stopped"),
            "a stopped session is tagged near the title: {title_row:?}"
        );
    }

    #[test]
    fn panel_leaves_a_playing_session_untagged() {
        let nps = NowPlayingState {
            current: Some(np("Song", "s", PlaybackStatus::Playing)),
            ..Default::default()
        };
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        draw_now_playing(&mut buf, area, &nps, false);
        let all = buffer_text(&buf);
        assert!(
            !all.contains("paused") && !all.contains("stopped"),
            "a playing session carries no status tag: {all:?}"
        );
    }
}
