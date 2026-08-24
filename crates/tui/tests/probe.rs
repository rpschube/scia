//! Capability-probe tests: the pure reply parsers, the environment classifiers
//! and the default-tier ladder. All run with no TTY (they never call
//! [`scia_tui::probe`], only its pure building blocks), so they are safe in CI.
//!
//! The recorded replies come from probe P4 (`docs/probes/p4-wt-sixel-frame-rate.md`):
//! DA1 `?61;4;…c` (attribute 4 = sixel), DECRQM 2026 → `2$y`, cell size 20×10 px.

use scia_tui::{
    CapabilityReport, SyncSupport, TermFamily, Tier, classify_family, default_tier,
    parse_cell_size, parse_da1, parse_decrqm_2026, truecolor_from,
};

// ---------------------------------------------------------------------------
// DA1 / sixel
// ---------------------------------------------------------------------------

#[test]
fn da1_reply_lists_attributes_including_sixel() {
    // The exact DA1 reply recorded on Windows Terminal in P4.
    let reply = "\x1b[?61;4;6;7;14;21;22;23;24;28;32;42;52c";
    let da1 = parse_da1(reply).expect("DA1 parses");
    assert_eq!(da1.attrs[0], 61, "leading terminal class");
    assert!(da1.attrs.contains(&4), "attribute 4 = sixel present");
    assert!(da1.attrs.contains(&52), "trailing attribute retained");
}

#[test]
fn da1_without_sixel_attribute() {
    let da1 = parse_da1("\x1b[?62;22c").expect("parses");
    assert!(!da1.attrs.contains(&4), "no sixel attribute");
}

#[test]
fn da1_found_after_a_kitty_apc_reply() {
    // A kitty terminal answers the graphics APC before DA1; the parser must
    // still find DA1 in the combined stream.
    let reply = "\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c";
    let da1 = parse_da1(reply).expect("DA1 after APC");
    assert!(da1.attrs.contains(&4));
}

#[test]
fn da1_malformed_or_empty_is_none() {
    assert!(parse_da1("").is_none());
    assert!(parse_da1("no escape here").is_none());
    assert!(parse_da1("\x1b[?61;4;xyzc").is_none(), "non-numeric field");
    assert!(parse_da1("\x1b[?61;4;6").is_none(), "no terminator");
}

// ---------------------------------------------------------------------------
// DECRQM 2026 / synchronized output
// ---------------------------------------------------------------------------

#[test]
fn decrqm_2026_set_and_reset_are_supported() {
    // P4 recorded `2$y` (mode recognized, currently reset).
    assert_eq!(
        parse_decrqm_2026("\x1b[?2026;2$y"),
        Some(SyncSupport::Supported)
    );
    assert_eq!(
        parse_decrqm_2026("\x1b[?2026;1$y"),
        Some(SyncSupport::Supported),
        "value 1 = set is also support"
    );
}

#[test]
fn decrqm_2026_zero_and_four_are_unsupported() {
    assert_eq!(
        parse_decrqm_2026("\x1b[?2026;0$y"),
        Some(SyncSupport::Unsupported),
        "value 0 = not recognized"
    );
    assert_eq!(
        parse_decrqm_2026("\x1b[?2026;4$y"),
        Some(SyncSupport::Unsupported),
        "value 4 = permanently reset"
    );
}

#[test]
fn decrqm_wrong_mode_or_malformed_is_none() {
    assert!(
        parse_decrqm_2026("\x1b[?1049;1$y").is_none(),
        "a different mode"
    );
    assert!(
        parse_decrqm_2026("\x1b[?2026;9$y").is_none(),
        "unknown value"
    );
    assert!(parse_decrqm_2026("").is_none());
    assert!(
        parse_decrqm_2026("\x1b[?2026;2y").is_none(),
        "no $ terminator"
    );
}

// ---------------------------------------------------------------------------
// Cell size
// ---------------------------------------------------------------------------

#[test]
fn cell_size_reports_height_then_width() {
    // P4 recorded a 20×10 px cell; the report is (height, width).
    assert_eq!(parse_cell_size("\x1b[6;20;10t"), Some((20, 10)));
}

#[test]
fn cell_size_found_after_a_da1_reply() {
    // In a combined buffer the DA1 `\x1b[?…` must not be mistaken for the cell
    // report.
    let reply = "\x1b[?61;4;6c\x1b[6;20;10t";
    assert_eq!(parse_cell_size(reply), Some((20, 10)));
}

#[test]
fn cell_size_malformed_or_empty_is_none() {
    assert!(parse_cell_size("").is_none());
    assert!(parse_cell_size("\x1b[6;20t").is_none(), "missing width");
    assert!(
        parse_cell_size("\x1b[6;20;xt").is_none(),
        "non-numeric width"
    );
    assert!(parse_cell_size("\x1b[6;20;10").is_none(), "no terminator");
}

// ---------------------------------------------------------------------------
// Terminal family from environment
// ---------------------------------------------------------------------------

#[test]
fn family_windows_terminal_from_wt_session() {
    let f = classify_family(Some("abc-123"), None, None, None, Some("xterm-256color"));
    assert_eq!(f, TermFamily::WindowsTerminal);
}

#[test]
fn family_ghostty_from_term_program_or_resources_dir() {
    assert_eq!(
        classify_family(None, Some("ghostty"), None, None, None),
        TermFamily::Ghostty
    );
    assert_eq!(
        classify_family(None, None, Some("/opt/ghostty"), None, None),
        TermFamily::Ghostty
    );
}

#[test]
fn family_kitty_from_window_id_or_term() {
    assert_eq!(
        classify_family(None, None, None, Some("1"), None),
        TermFamily::Kitty
    );
    assert_eq!(
        classify_family(None, None, None, None, Some("xterm-kitty")),
        TermFamily::Kitty
    );
}

#[test]
fn family_other_when_nothing_matches() {
    assert_eq!(
        classify_family(None, Some("iTerm.app"), None, None, Some("xterm-256color")),
        TermFamily::Other
    );
}

#[test]
fn family_precedence_wt_beats_everything() {
    // WT_SESSION wins even when kitty/ghostty signals are also present.
    let f = classify_family(
        Some("s"),
        Some("ghostty"),
        None,
        Some("1"),
        Some("xterm-kitty"),
    );
    assert_eq!(f, TermFamily::WindowsTerminal);
}

// ---------------------------------------------------------------------------
// Truecolor
// ---------------------------------------------------------------------------

#[test]
fn truecolor_from_colorterm_variants() {
    assert!(truecolor_from(Some("truecolor"), TermFamily::Other));
    assert!(truecolor_from(Some("24bit"), TermFamily::Other));
    assert!(!truecolor_from(Some("256color"), TermFamily::Other));
    assert!(!truecolor_from(None, TermFamily::Other));
}

#[test]
fn truecolor_implied_by_known_family() {
    assert!(truecolor_from(None, TermFamily::WindowsTerminal));
    assert!(truecolor_from(None, TermFamily::Ghostty));
    assert!(truecolor_from(None, TermFamily::Kitty));
}

// ---------------------------------------------------------------------------
// Default tier ladder
// ---------------------------------------------------------------------------

/// A report carrying only a family; the other facts do not affect the tier.
fn report_for(family: TermFamily) -> CapabilityReport {
    CapabilityReport {
        truecolor: true,
        sixel: false,
        sync_2026: false,
        cell_px: None,
        kitty_graphics: false,
        family,
    }
}

#[test]
fn default_tier_table() {
    // The P3-informed ladder start.
    assert_eq!(
        default_tier(&report_for(TermFamily::WindowsTerminal)),
        Tier::Quadrant,
        "WT: octants/sextants are font-dependent and commonly absent (P3)"
    );
    assert_eq!(
        default_tier(&report_for(TermFamily::Ghostty)),
        Tier::Octant,
        "Ghostty rasterizes legacy-computing glyphs itself"
    );
    assert_eq!(
        default_tier(&report_for(TermFamily::Kitty)),
        Tier::Octant,
        "kitty rasterizes legacy-computing glyphs itself"
    );
    assert_eq!(
        default_tier(&report_for(TermFamily::Other)),
        Tier::Half,
        "unknown terminals fall back to the safe half-block rung"
    );
}

#[test]
fn report_display_is_one_compact_line() {
    let report = CapabilityReport {
        truecolor: true,
        sixel: true,
        sync_2026: true,
        cell_px: Some((20, 10)),
        kitty_graphics: false,
        family: TermFamily::WindowsTerminal,
    };
    let line = report.to_string();
    assert!(!line.contains('\n'), "single line");
    assert!(line.contains("windows-terminal"));
    assert!(line.contains("sixel=yes"));
    assert!(line.contains("cell=10x20px"), "width x height");
}
