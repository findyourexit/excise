use ::std::fs::{File, create_dir};
use ::std::io::prelude::*;
use ::std::path::Path;

use crossterm::event::KeyModifiers;
use crossterm::event::{Event, KeyCode, KeyEvent};
use unicode_width::UnicodeWidthStr as _;

use crate::start;
use crate::tests::cases::test_utils::*;
use crate::tests::fakes::TerminalEvents;
use crate::tests::fixtures::TestDirectory;

macro_rules! key {
    (char $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    };
    (ctrl $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::CONTROL,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    };
    ($x:ident) => {
        Event::Key(KeyEvent {
            code: KeyCode::$x,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    };
}

fn metric_value_range(line: &str, marker: &str) -> Option<std::ops::Range<usize>> {
    let marker_end = line.find(marker)?.saturating_add(marker.len());
    let value_start = marker_end.saturating_add(
        line.get(marker_end..)?
            .find(|character: char| !character.is_ascii_whitespace())?,
    );
    let value_end = value_start.saturating_add(
        line.get(value_start..)?
            .find(|character: char| character.is_whitespace() || matches!(character, '│' | '║'))
            .unwrap_or_else(|| line.len().saturating_sub(value_start)),
    );
    Some(value_start..value_end)
}

const CANONICAL_IDENTITY: &str = "Inode { device_id: ########, inode_number: ######## }";
const IDENTITY_VARIANTS: [&str; 3] = ["Inode", "LowRes", "HighRes"];

/// Canonical identity text, fitted to the width the inspector cell offers.
///
/// The value is ASCII, so character count and display width agree.
fn canonical_identity(width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if CANONICAL_IDENTITY.width() <= width {
        let mut value = CANONICAL_IDENTITY.to_string();
        value.extend(std::iter::repeat_n(' ', width - CANONICAL_IDENTITY.width()));
        return value;
    }
    let mut value: String = CANONICAL_IDENTITY.chars().take(width - 1).collect();
    value.push('…');
    value
}

/// Returns the byte length of an inspector middle-truncation marker.
fn identity_truncation_marker_len(value: &str) -> Option<usize> {
    if value.starts_with("[...]") {
        Some("[...]".len())
    } else {
        value.starts_with("[..]").then_some("[..]".len())
    }
}

/// Returns the byte length of a canonical identity at the front of a cell.
///
/// Snapshots are normalised before `insta` compares them, so this must accept
/// both the full placeholder value and a prefix which ends at its ellipsis.
/// Keeping that exception exact prevents an ASCII grid overlay from becoming
/// part of the identity merely because a complete variant came before it.
fn canonical_identity_end(value: &str) -> Option<usize> {
    if let Some(remainder) = value.strip_prefix(CANONICAL_IDENTITY) {
        let padding = remainder
            .chars()
            .take_while(|character| *character == ' ')
            .map(char::len_utf8)
            .sum::<usize>();
        return Some(CANONICAL_IDENTITY.len() + padding);
    }

    let prefix_len = value
        .bytes()
        .zip(CANONICAL_IDENTITY.bytes())
        .take_while(|(actual, expected)| actual == expected)
        .count();
    (prefix_len > 0
        && value
            .get(prefix_len..)
            .is_some_and(|remainder| remainder.starts_with('…')))
    .then_some(prefix_len + '…'.len_utf8())
}

/// Returns the byte offset where a rendered identity cell ends.
///
/// `PANE_BORDER_SET` uses `▟ ▜ ▔ ▏ ▕ ▐ ▌`, and `dense_grid` uses
/// `▀ ░ ▒ ▓ █ ·`, so any non-ASCII glyph ends a cell unless it immediately
/// follows an exact inspector truncation marker. ASCII grid shades do too.
/// Canonical normalizer output is recognised separately above, which keeps its
/// placeholder hashes and ellipsis idempotent without treating overlay paint
/// as identity text.
fn identity_cell_content_end(value: &str, structured_identity: bool) -> usize {
    if let Some(end) = canonical_identity_end(value) {
        return end;
    }

    let mut end = 0;
    let mut after_truncation_marker = false;
    while let Some(rest) = value.get(end..) {
        let Some(character) = rest.chars().next() else {
            break;
        };
        if let Some(marker_len) = identity_truncation_marker_len(rest) {
            end += marker_len;
            after_truncation_marker = true;
            continue;
        }
        if !character.is_ascii() {
            if character == '…' && (after_truncation_marker || structured_identity) {
                end += character.len_utf8();
                continue;
            }
            break;
        }
        if character == '|' {
            break;
        }
        if structured_identity {
            if matches!(character, '-' | '=' | '+' | '#' | '.') {
                break;
            }
            after_truncation_marker = false;
            end += character.len_utf8();
            continue;
        }
        if matches!(character, '\n' | '\r' | '-' | '=' | '+' | '#' | '.') {
            break;
        }
        after_truncation_marker = false;
        end += character.len_utf8();
    }
    end
}

/// Reports whether a partial variant is interrupted by an actual truncation marker.
fn has_truncated_identity_variant(value: &str) -> bool {
    IDENTITY_VARIANTS.iter().any(|variant| {
        (1..variant.len()).any(|prefix_len| {
            value
                .strip_prefix(&variant[..prefix_len])
                .is_some_and(|rest| identity_truncation_marker_len(rest).is_some())
        })
    })
}

fn has_identity_overlay_after(value: &str, content_end: usize) -> bool {
    value.get(content_end..).is_some_and(|suffix| {
        suffix
            .chars()
            .find(|character| !character.is_whitespace())
            .is_some_and(|character| {
                matches!(
                    character,
                    '▔' | '▟'
                        | '▜'
                        | '▏'
                        | '▐'
                        | '▌'
                        | '░'
                        | '▒'
                        | '▓'
                        | '█'
                        | '·'
                        | '-'
                        | '='
                        | '+'
                        | '#'
                        | '.'
                )
            })
    })
}

/// Reports whether the marker begins immediately inside an inspector or modal cell.
///
/// A map label can use the same words as a filesystem identity, but it cannot
/// begin directly after the cell's left vertical frame.
fn is_identity_cell_left_edge(character: char) -> bool {
    matches!(character, '▏' | '│' | '║' | '|')
}

/// Replaces an identity cell with one canonical value of the same width.
///
/// Identity carries filesystem numbers whose digit counts vary by machine, and
/// Windows reports a different identity variant entirely, so the inspector's
/// truncation lands on a different character on every platform. An overlay can
/// also clip the cell down to one character of the variant name, which is
/// still `I` on Unix and `L` or `H` on Windows. Replacing the whole cell rather
/// than masking digits inside it is what keeps these frames comparable across
/// the targets CI runs.
fn normalize_identity_cell(line: &str) -> String {
    let Some((start, marker, end)) = ["identity  ", "identity "].into_iter().find_map(|marker| {
        let start = line.find(marker)?;
        let value_start = start + marker.len();
        let rest = line.get(value_start..)?;
        let has_complete_variant = IDENTITY_VARIANTS
            .iter()
            .any(|variant| rest.starts_with(variant));
        let has_truncated_variant = has_truncated_identity_variant(rest);
        let structured_identity = IDENTITY_VARIANTS.iter().any(|variant| {
            rest.strip_prefix(variant)
                .is_some_and(|suffix| suffix.starts_with(" {"))
        }) || has_truncated_variant;
        let content_end = identity_cell_content_end(rest, structured_identity);
        let end = value_start + content_end;
        let value = line.get(value_start..end)?.trim_end();
        let has_abbreviated_variant = !has_complete_variant
            && IDENTITY_VARIANTS
                .iter()
                .any(|variant| variant.starts_with(value))
            && has_identity_overlay_after(rest, content_end);
        let is_inspector_or_modal_cell = line[..start]
            .chars()
            .next_back()
            .is_some_and(is_identity_cell_left_edge);
        (!value.is_empty()
            && is_inspector_or_modal_cell
            && (has_complete_variant || has_abbreviated_variant || has_truncated_variant))
            .then_some((start, marker, end))
    }) else {
        return line.to_string();
    };
    let cell_width = line[start..end].width();

    let mut canonical = String::with_capacity(cell_width);
    canonical.push_str(marker);
    canonical.push_str(&canonical_identity(
        cell_width.saturating_sub(marker.width()),
    ));

    let mut normalized = String::with_capacity(line.len());
    normalized.push_str(&line[..start]);
    normalized.push_str(&canonical);
    normalized.push_str(&line[end..]);
    normalized
}

fn normalize_snapshot(frame: &str) -> String {
    let mut normalized = String::with_capacity(frame.len());
    for raw_line in frame.split_inclusive('\n') {
        let line = normalize_identity_cell(raw_line);
        let identity_start = (line != raw_line)
            .then(|| {
                line.find("identity ")
                    .map(|index| index + "identity ".len())
            })
            .flatten()
            .unwrap_or(usize::MAX);
        let links_start = line
            .find("links ")
            .map_or(usize::MAX, |index| index + "links ".len());
        let metric_ranges = [
            metric_value_range(&line, "allocated "),
            metric_value_range(&line, "reclaim "),
        ];
        let mut in_identity_or_links_number = false;
        for (index, character) in line.char_indices() {
            let dynamic_metric = metric_ranges
                .iter()
                .flatten()
                .any(|range| range.contains(&index));
            let in_identity = index >= identity_start;
            let in_links = index >= links_start;
            if character.is_ascii_digit() && in_identity {
                if !in_identity_or_links_number {
                    normalized.push_str("########");
                    in_identity_or_links_number = true;
                }
            } else if character.is_ascii_digit() && in_links {
                if !in_identity_or_links_number {
                    normalized.push('#');
                    in_identity_or_links_number = true;
                }
            } else {
                in_identity_or_links_number = false;
                if character.is_ascii_digit() && dynamic_metric {
                    normalized.push('#');
                } else {
                    normalized.push(character);
                }
            }
        }
    }
    normalized
}

/// The identity cell is the one place a frame carries machine-specific text:
/// Unix reports an inode, Windows a file index, and their digit counts differ,
/// so an unnormalized cell makes these snapshots unportable. CI runs all three
/// targets against the same committed frames.
#[test]
fn identity_cells_read_the_same_on_every_platform() {
    let unix = "▏identity  Inode { device_id: 16777232, inode_number: 1234567 }  ▕";
    let windows = "▏identity  LowRes { volume_serial: 9, file_index: 88 }           ▕";
    assert_eq!(unix.width(), windows.width(), "fixtures must share a width");
    assert_eq!(normalize_snapshot(unix), normalize_snapshot(windows));

    // An overlay can clip the cell down to one character of the variant name.
    let unix_clipped = "▏identity  I▔▔▔▔▔▔▔▔▔▔▕";
    let windows_clipped = "▏identity  L▔▔▔▔▔▔▔▔▔▔▕";
    assert_eq!(
        normalize_snapshot(unix_clipped),
        normalize_snapshot(windows_clipped)
    );
    assert!(normalize_snapshot(unix_clipped).contains("identity  …▔"));
    let bare_prefix = "▏identity  I          ▕";
    assert_eq!(
        normalize_snapshot(bare_prefix),
        bare_prefix,
        "a bare variant prefix must not be treated as clipped identity"
    );
    let neighboring_paint = "▏identity  I          ▕▓▓";
    assert_eq!(
        normalize_snapshot(neighboring_paint),
        neighboring_paint,
        "paint after the closing border is outside the identity cell"
    );
}

/// ASCII overlays reuse the map's shades and grain, so they must end an
/// identity cell before the platform-specific variant can be identified.
#[test]
fn identity_cells_stop_at_ascii_chrome() {
    let unix = "|identity  I----------|";
    let low_res = "|identity  L----------|";
    let high_res = "|identity  H----------|";

    assert_eq!(normalize_snapshot(unix), normalize_snapshot(low_res));
    assert_eq!(normalize_snapshot(unix), normalize_snapshot(high_res));
    assert_eq!(normalize_snapshot(unix), "|identity  …----------|");

    for chrome in ['=', '+', '#', '.'] {
        let unix = format!("|identity  I{chrome}|");
        let windows = format!("|identity  L{chrome}|");

        assert_eq!(
            normalize_snapshot(&unix),
            normalize_snapshot(&windows),
            "{chrome} must end the identity cell"
        );
    }
}

/// Complete variants still end where an ASCII grid overlay begins. Canonical
/// placeholder text is the sole `#`-bearing identity value that remains valid.
#[test]
fn complete_identity_variants_stop_before_ascii_chrome() {
    let unix = "|identity  Inode ----------|";
    let windows = "|identity  LowRes----------|";
    assert_eq!(unix.width(), windows.width(), "fixtures must share a width");

    let normalized = normalize_snapshot(unix);
    assert_eq!(normalized, normalize_snapshot(windows));
    assert_eq!(normalized, "|identity  Inode…----------|");
    assert_eq!(normalized.width(), unix.width());
    assert_eq!(normalize_snapshot(&normalized), normalized);

    let clipped = "|identity  Inode----------|";
    assert_eq!(normalize_snapshot(clipped), "|identity  Inod…----------|");

    let canonical = "|identity  Inode { device_id: ########, inode_number: ######## }----------|";
    assert_eq!(normalize_snapshot(canonical), canonical);
}

/// Raw structured identities can be obscured before their closing brace. The
/// ASCII overlay must stay observable rather than being replaced as identity
/// content.
#[test]
fn open_structured_identity_cells_stop_before_ascii_chrome() {
    for chrome in ['-', '=', '+', '#', '.'] {
        // The spacer makes the otherwise different variant names the same width.
        let unix = format!("|identity  Inode {{ x: 1 {chrome}overlay|");
        let windows = format!("|identity  LowRes {{ x: 1{chrome}overlay|");
        assert_eq!(unix.width(), windows.width(), "fixtures must share a width");

        let normalized = normalize_snapshot(&unix);
        assert_eq!(normalized, normalize_snapshot(&windows));
        assert!(
            normalized.ends_with(&format!("{chrome}overlay|")),
            "{chrome} overlay must remain visible: {normalized}"
        );
        assert_eq!(normalize_snapshot(&normalized), normalized);
    }
}

/// The inspector middle-truncates a value it cannot fit, and `truncate_middle`
/// spells that elision `[...]`. Those dots sit inside the identity, so a cell
/// that stops on them keeps the machine's own digits either side of the marker
/// and the frame stops being portable, which is the one thing normalising is here for.
#[test]
fn middle_truncated_identity_cells_normalise_whole() {
    let unix = "▏identity  Inode { device_i[...]32, inode_number: 1234567 }▕";
    let windows = "▏identity  LowRes { volume_[...]l: 9, file_index: 8888888 }▕";
    assert_eq!(unix.width(), windows.width(), "fixtures must share a width");

    let normalized = normalize_snapshot(unix);
    assert_eq!(normalized, normalize_snapshot(windows));
    assert_eq!(
        normalized.width(),
        unix.width(),
        "a normalised cell must still fit the frame it came from"
    );
    assert!(
        !normalized.contains("[...]"),
        "the whole cell is replaced, marker included: {normalized}"
    );
}

/// A narrow inspector may truncate inside a variant name before it can identify
/// the platform-specific identity type. Its exact elision marker is enough to
/// identify the cell without mistaking similarly named map entries for one.
#[test]
fn variant_prefixes_truncated_by_markers_normalise_whole_cells() {
    for (unix, windows) in [
        ("▏identity  Ino[...]…▕", "▏identity  Low[...]…▕"),
        ("▏identity  Ino[..]…▕", "▏identity  Low[..]…▕"),
    ] {
        assert_eq!(unix.width(), windows.width(), "fixtures must share a width");

        let normalized = normalize_snapshot(unix);
        assert_eq!(normalized, normalize_snapshot(windows));
        assert_eq!(normalized.width(), unix.width());
        assert!(!normalized.contains('['));
        assert_eq!(normalize_snapshot(&normalized), normalized);
    }
}

/// A map filename may look like a complete, abbreviated, or truncated
/// filesystem identity but has no inspector or modal cell around it, so its
/// rendered text remains observable.
#[test]
fn map_entries_named_like_identities_remain_visible() {
    let map_entry = "▓▓identity I▓▓";
    let truncated_map_entry = "▓▓identity Ino[...]…▓▓";

    assert_eq!(normalize_snapshot(map_entry), map_entry);
    assert_eq!(normalize_snapshot(truncated_map_entry), truncated_map_entry);
    for complete_map_entry in [
        "▓▓identity Inode secret▓▓",
        "▓▓identity LowRes secret▓▓",
        "▓▓identity HighRes secret▓▓",
        "▓▓identity Inode 123 secret▓▓",
    ] {
        assert_eq!(
            normalize_snapshot(complete_map_entry),
            complete_map_entry,
            "map text must remain byte-for-byte unchanged"
        );
    }
}

macro_rules! assert_snapshot {
    ($value:expr) => {{
        let normalized = normalize_snapshot($value);
        ::insta::assert_snapshot!(normalized);
    }};
}
// this means we ask excise to show the actual file size rather than the size taken on disk
//
// this is in order to make the tests more possible, so they will show the same result
// on filesystems with and without compression
const SHOW_APPARENT_SIZE: bool = true;

// This leaves delete confirmations enabled (The default behaviour).
const DELETE_CONFIRMATION_ENABLED: bool = false;
const DELETE_CONFIRMATION_DISABLED: bool = true;

fn create_root_temp_dir(name: &str) -> anyhow::Result<TestDirectory> {
    TestDirectory::new(name)
}

fn create_temp_file<P: AsRef<Path>>(path: P, size: usize) -> Result<(), anyhow::Error> {
    let mut file = File::create(path)?;
    let mut pos = 0;
    while pos < size {
        let bytes_written = file.write(b"W")?;
        pos += bytes_written;
    }
    Ok(())
}

fn assert_compact_inspector(frame: &str) {
    for expected in [
        "INSPECT",
        "allocated",
        "reclaim",
        "entries",
        "identity",
        "links",
    ] {
        assert!(
            frame.contains(expected),
            "narrow selection omitted {expected}"
        );
    }
}

#[test]
fn two_large_files_one_small_file() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("two_large_files_one_small_file").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn medium_width() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(60, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path = create_root_temp_dir("medium_width").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn list_title_uses_the_workspace_inner_width() {
    for width in [72, 73] {
        let (terminal_events, terminal_draw_events, backend) = test_backend_factory(width, 50);
        let keyboard_events = Box::new(wait_and_quit_events(1, true));
        let temp_dir_path = create_root_temp_dir(&format!("list_title_at_{width}"))
            .expect("failed to create temp dir");
        create_temp_file(temp_dir_path.path().join("file"), 4096)
            .expect("failed to create temp file");

        start(
            backend,
            keyboard_events,
            temp_dir_path.path().to_path_buf(),
            SHOW_APPARENT_SIZE,
            DELETE_CONFIRMATION_ENABLED,
        );
        drop(temp_dir_path);

        assert_terminal_lifecycle(
            &terminal_events
                .lock()
                .expect("failed to lock test terminal events"),
        );
        let frames = terminal_draw_events
            .lock()
            .expect("failed to lock test draw events");
        assert!(
            frames.iter().any(|frame| frame.contains(" LIST ")),
            "the {width}-column list must be labelled LIST"
        );
        assert!(
            frames.iter().all(|frame| !frame.contains(" STORAGE MAP ")),
            "the {width}-column list must not be labelled STORAGE MAP"
        );
    }
}

#[test]
fn small_width() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(50, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path = create_root_temp_dir("small_width").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn small_width_long_folder_name() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(50, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("small_width_long_folder_name").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn too_small_width_one() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(49, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("too_small_width_one").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
}

#[test]
fn too_small_width_two() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(26, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, false));
    let temp_dir_path =
        create_root_temp_dir("too_small_width_two").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
}

#[test]
fn too_small_width_three() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(20, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, false));
    let temp_dir_path =
        create_root_temp_dir("too_small_width_three").expect("failed to create temp dir");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
}

#[test]
fn too_small_width_four() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(15, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, false));
    let temp_dir_path =
        create_root_temp_dir("too_small_width_four").expect("failed to create temp dir");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
}

#[test]
fn too_small_width_five() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(5, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, false));
    let temp_dir_path =
        create_root_temp_dir("too_small_width_five").expect("failed to create temp dir");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
}

#[test]
fn too_small_height() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 14);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("too_small_height").expect("failed to create temp dir");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
}

#[test]
fn eleven_files() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path = create_root_temp_dir("eleven_files").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 8192).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    let mut file_4_path = temp_dir_path.path().to_path_buf();
    file_4_path.push("file4");
    create_temp_file(file_4_path, 8192).expect("failed to create temp file");

    let mut file_5_path = temp_dir_path.path().to_path_buf();
    file_5_path.push("file5");
    create_temp_file(file_5_path, 8192).expect("failed to create temp file");

    let mut file_6_path = temp_dir_path.path().to_path_buf();
    file_6_path.push("file6");
    create_temp_file(file_6_path, 53248).expect("failed to create temp file");

    let mut file_7_path = temp_dir_path.path().to_path_buf();
    file_7_path.push("file7");
    create_temp_file(file_7_path, 151_552).expect("failed to create temp file");

    let mut file_8_path = temp_dir_path.path().to_path_buf();
    file_8_path.push("file8");
    create_temp_file(file_8_path, 53248).expect("failed to create temp file");

    let mut file_9_path = temp_dir_path.path().to_path_buf();
    file_9_path.push("file9");
    create_temp_file(file_9_path, 53248).expect("failed to create temp file");

    let mut file_10_path = temp_dir_path.path().to_path_buf();
    file_10_path.push("file10");
    create_temp_file(file_10_path, 53248).expect("failed to create temp file");

    let mut file_11_path = temp_dir_path.path().to_path_buf();
    file_11_path.push("file11");
    create_temp_file(file_11_path, 53248).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn enter_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("enter_folder").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 8192).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
}

#[test]
fn enter_folder_medium_width() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(90, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("enter_folder_medium_width").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 8192).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    let selected_frame = &terminal_draw_events_mirror[1];
    for expected in [
        "INSPECT",
        "allocated",
        "reclaim",
        "entries",
        "identity",
        "links",
        "scope",
    ] {
        assert!(
            selected_frame.contains(expected),
            "medium-width selection omitted {expected}"
        );
    }
    assert_snapshot!(&terminal_draw_events_mirror[2]);
}

#[test]
fn enter_folder_small_width() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(60, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("enter_folder_small_width").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder_with_quite_a_long_name");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder_with_quite_a_long_name");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 8192).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_compact_inspector(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
}

#[test]
fn small_files() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path = create_root_temp_dir("small_files").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 401_408).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 1_000_000).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 1_000_000).expect("failed to create temp file");

    let mut file_4_path = temp_dir_path.path().to_path_buf();
    file_4_path.push("file4");
    create_temp_file(file_4_path, 8192).expect("failed to create temp file");

    let mut file_5_path = temp_dir_path.path().to_path_buf();
    file_5_path.push("file5");
    create_temp_file(file_5_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn zoom_into_small_files() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 2).collect();
    events.push(Some(key!(char '+')));
    events.push(None);
    events.push(Some(key!(char '+')));
    events.push(None);
    events.push(Some(key!(char '+')));
    events.push(None);
    events.push(Some(key!(char '+')));
    events.push(None);
    events.push(Some(key!(char '-')));
    events.push(None);
    events.push(Some(key!(char '-')));
    events.push(None);
    events.push(Some(key!(char '0')));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));
    let temp_dir_path =
        create_root_temp_dir("zoom_into_small_files").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 401_408).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 1_000_000).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 1_000_000).expect("failed to create temp file");

    let mut file_4_path = temp_dir_path.path().to_path_buf();
    file_4_path.push("file4");
    create_temp_file(file_4_path, 8192).expect("failed to create temp file");

    let mut file_5_path = temp_dir_path.path().to_path_buf();
    file_5_path.push("file5");
    create_temp_file(file_5_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
    assert_snapshot!(&terminal_draw_events_mirror[7]);
}

#[test]
fn cannot_move_into_small_files() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 2).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("cannot_move_into_small_files").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 401_408).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 1_000_000).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 1_000_000).expect("failed to create temp file");

    let mut file_4_path = temp_dir_path.path().to_path_buf();
    file_4_path.push("file4");
    create_temp_file(file_4_path, 4096).expect("failed to create temp file");

    let mut file_5_path = temp_dir_path.path().to_path_buf();
    file_5_path.push("file5");
    create_temp_file(file_5_path, 4096).expect("failed to create temp file");

    let mut file_6_path = temp_dir_path.path().to_path_buf();
    file_6_path.push("file6");
    create_temp_file(file_6_path, 4096).expect("failed to create temp file");

    let mut file_7_path = temp_dir_path.path().to_path_buf();
    file_7_path.push("file7");
    create_temp_file(file_7_path, 4096).expect("failed to create temp file");

    let mut file_8_path = temp_dir_path.path().to_path_buf();
    file_8_path.push("file8");
    create_temp_file(file_8_path, 4096).expect("failed to create temp file");

    let mut file_9_path = temp_dir_path.path().to_path_buf();
    file_9_path.push("file9");
    create_temp_file(file_9_path, 4096).expect("failed to create temp file");

    let mut file_10_path = temp_dir_path.path().to_path_buf();
    file_10_path.push("file10");
    create_temp_file(file_10_path, 4096).expect("failed to create temp file");

    let mut file_11_path = temp_dir_path.path().to_path_buf();
    file_11_path.push("file11");
    create_temp_file(file_11_path, 4096).expect("failed to create temp file");

    let mut file_12_path = temp_dir_path.path().to_path_buf();
    file_12_path.push("file12");
    create_temp_file(file_12_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
}

#[test]
fn minimum_tile_sides() {
    // here we test that tiles are not created with a side_length (height in this case)
    // that is too small to render while not being designated as a "small file"
    //
    // the only case in which this can happen if this is the last tile to be placed
    // this case might in the future be solved by artificially increasing its size
    // to the minimum with some sort of asterisk to explain

    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("minimum_tile_sides").expect("failed to create temp dir");

    for i in 0..7 {
        let mut file_path = temp_dir_path.path().to_path_buf();
        file_path.push(format!("big_file{i}"));
        create_temp_file(file_path, 135_168).expect("failed to create temp file");
    }

    for i in 0..2 {
        let mut file_path = temp_dir_path.path().to_path_buf();
        file_path.push(format!("medium_file{i}"));
        create_temp_file(file_path, 8192).expect("failed to create temp file");
    }

    for i in 0..50 {
        let mut file_path = temp_dir_path.path().to_path_buf();
        file_path.push(format!("file{i}"));
        create_temp_file(file_path, 4096).expect("failed to create temp file");
    }

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn move_down_and_enter_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 2).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("move_down_and_enter_folder").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("subfolder1");
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
}

#[test]
fn noop_when_entering_file() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("noop_when_entering_file").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 8192).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
}

#[test]
fn move_up_and_enter_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(char 'k')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("move_up_and_enter_folder").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 12288).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 8192).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
}

#[test]
fn move_right_and_enter_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("move_right_and_enter_folder").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
}

#[test]
fn move_left_and_enter_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(char 'h')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("move_left_and_enter_folder").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 8192).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
}

#[test]
fn enter_opens_the_biggest_entry_a_fresh_map_points_at() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(Some(key!(char '\n')));
    // Extra idle frames so the drill transition plays out before ctrl-c ends the
    // session: the frames under test are the ones during the movement.
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("enter_opens_the_biggest_entry_a_fresh_map_points_at")
        .expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 8192).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
}

#[test]
fn the_cursor_holds_at_the_map_edge() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("noop_when_moving_off_screen_edges")
        .expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
}

#[test]
fn esc_to_go_up() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    events.push(None);
    events.push(Some(key!(Esc)));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("esc_to_go_up").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
}

#[test]
fn noop_when_pressing_esc_at_base_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(char '\n')));
    events.push(None);
    events.push(Some(key!(Esc)));
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(Esc)));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("noop_when_pressing_esc_at_base_folder")
        .expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
}

#[test]
fn delete_file() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'y')));
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("delete_file").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&file_2_path).is_err(),
        "file successfully deleted"
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "different folder stayed the same"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untoucehd"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
}

#[test]
fn delete_file_no_confirmation() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'y')));
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("delete_file_no_confirmation").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_DISABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&file_2_path).is_err(),
        "file successfully deleted"
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "different folder stayed the same"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untoucehd"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
}

#[test]
fn cant_delete_file_with_term_too_small() {
    let (terminal_events, _terminal_draw_events, backend) = test_backend_factory(49, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(Esc)));
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("cant_delete_file_with_term_too_small")
        .expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(std::fs::metadata(&file_2_path).is_ok(), "file not deleted");
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "different folder stayed the same"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untoucehd"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);
}

#[test]
fn delete_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(Enter)));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("delete_folder").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_err(),
        "folder successfully deleted"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_err(),
        "internal file successfully deleted"
    ); // can't really fail on its own, but left here for clarity
    assert!(
        std::fs::metadata(&file_2_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
    assert_snapshot!(&terminal_draw_events_mirror[7]);
}

#[test]
fn delete_folder_no_confirmation() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(Backspace)));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(Some(key!(char 'y')));
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("delete_folder_no_confirmation").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_DISABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_err(),
        "folder successfully deleted"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_err(),
        "internal file successfully deleted"
    ); // can't really fail on its own, but left here for clarity
    assert!(
        std::fs::metadata(&file_2_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
}

#[test]
fn delete_folder_small_window() {
    // terminal window with a width of 60 (shorter message window layout)
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(60, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(Enter)));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("delete_folder_small_window").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&file_2_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_err(),
        "file successfully deleted"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_err(),
        "file in folder deleted"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_compact_inspector(&terminal_draw_events_mirror[1]);
    assert_compact_inspector(&terminal_draw_events_mirror[2]);
    assert_compact_inspector(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
    assert_snapshot!(&terminal_draw_events_mirror[7]);
    assert_snapshot!(&terminal_draw_events_mirror[8]);
}

#[test]
fn delete_folder_small_window_no_confirmation() {
    // terminal window with a width of 60 (shorter message window layout)
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(60, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(char 'j')));
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'y')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("delete_folder_small_window_no_confirmation")
        .expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_DISABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&file_2_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_err(),
        "file successfully deleted"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_err(),
        "file in folder deleted"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_compact_inspector(&terminal_draw_events_mirror[1]);
    assert_compact_inspector(&terminal_draw_events_mirror[2]);
    assert_compact_inspector(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
    assert_snapshot!(&terminal_draw_events_mirror[7]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn delete_folder_with_multiple_children() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(Enter)));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("delete_folder_with_multiple_children")
        .expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 16384).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 16384).expect("failed to create temp file");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut subfolder_2_path = temp_dir_path.path().to_path_buf();
    subfolder_2_path.push("subfolder1");
    subfolder_2_path.push("subfolder2");
    create_dir(&subfolder_2_path).expect("failed to create temporary directory");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("subfolder1");
    file_3_path.push("subfolder2");
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    let mut file_4_path = temp_dir_path.path().to_path_buf();
    file_4_path.push("subfolder1");
    file_4_path.push("subfolder2");
    file_4_path.push("file4");
    create_temp_file(&file_4_path, 4096).expect("failed to create temp file");

    let mut file_5_path = temp_dir_path.path().to_path_buf();
    file_5_path.push("subfolder1");
    file_5_path.push("file5");
    create_temp_file(&file_5_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_err(),
        "folder successfully deleted"
    );
    assert!(
        std::fs::metadata(&subfolder_2_path).is_err(),
        "folder inside deleted folder successfully deleted"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&file_2_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_err(),
        "internal file in folder deleted"
    );
    assert!(
        std::fs::metadata(&file_4_path).is_err(),
        "internal file in folder deleted"
    );
    assert!(
        std::fs::metadata(&file_5_path).is_err(),
        "internal file in folder deleted"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
    assert_snapshot!(&terminal_draw_events_mirror[7]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn delete_folder_with_multiple_children_no_confirmation() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char 'l')));
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'y')));
    // here we sleep extra to allow the blink events to happen and be tested before the app exits
    // with the following ctrl-c
    events.push(None);
    events.push(None);
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("delete_folder_with_multiple_children_no_confirmation")
            .expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 16384).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 16384).expect("failed to create temp file");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut subfolder_2_path = temp_dir_path.path().to_path_buf();
    subfolder_2_path.push("subfolder1");
    subfolder_2_path.push("subfolder2");
    create_dir(&subfolder_2_path).expect("failed to create temporary directory");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("subfolder1");
    file_3_path.push("subfolder2");
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    let mut file_4_path = temp_dir_path.path().to_path_buf();
    file_4_path.push("subfolder1");
    file_4_path.push("subfolder2");
    file_4_path.push("file4");
    create_temp_file(&file_4_path, 4096).expect("failed to create temp file");

    let mut file_5_path = temp_dir_path.path().to_path_buf();
    file_5_path.push("subfolder1");
    file_5_path.push("file5");
    create_temp_file(&file_5_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_DISABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(
        std::fs::metadata(&subfolder_1_path).is_err(),
        "folder successfully deleted"
    );
    assert!(
        std::fs::metadata(&subfolder_2_path).is_err(),
        "folder inside deleted folder successfully deleted"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&file_2_path).is_ok(),
        "different file was untouched"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_err(),
        "internal file in folder deleted"
    );
    assert!(
        std::fs::metadata(&file_4_path).is_err(),
        "internal file in folder deleted"
    );
    assert!(
        std::fs::metadata(&file_5_path).is_err(),
        "internal file in folder deleted"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
}

/// The map arms a cursor as soon as it has entries, so the first Backspace of a
/// session already has a target and raises the confirmation. Answering `n`
/// leaves the filesystem untouched.
#[test]
fn a_fresh_map_already_has_a_delete_target() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'n')));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("a_fresh_map_already_has_a_delete_target")
        .expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(std::fs::metadata(&file_2_path).is_ok(), "file not deleted");
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "different folder stayed the same"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untoucehd"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    let confirmation_index = terminal_draw_events_mirror
        .iter()
        .position(|frame| frame.contains("PERMANENT FILE DELETION"))
        .expect("fresh-map deletion should reach confirmation");
    terminal_draw_events_mirror
        .iter()
        .skip(confirmation_index.saturating_add(1))
        .find(|frame| {
            frame.contains("STORAGE MAP")
                && !frame.contains("PERMANENT FILE DELETION")
                && !frame.contains("BUILDING IDENTITY PLAN")
                && !frame.contains("Quit Excise?")
        })
        .expect("n should return to the normal map before quitting");
}

#[test]
fn delete_file_press_n() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'n')));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("delete_file_press_n").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(&file_2_path, 4096).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(&file_3_path, 4096).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );
    assert!(std::fs::metadata(&file_2_path).is_ok(), "file not deleted");
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "different folder stayed the same"
    );
    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "different file was untoucehd"
    );
    assert!(
        std::fs::metadata(&file_3_path).is_ok(),
        "second different file was untouched"
    );
    drop(temp_dir_path);

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
}

#[test]
fn files_with_size_zero() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("files_with_size_zero").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 0).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 0).expect("failed to create temp file");

    let mut file_3_path = temp_dir_path.path().to_path_buf();
    file_3_path.push("file3");
    create_temp_file(file_3_path, 0).expect("failed to create temp file");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn empty_folder() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path = create_root_temp_dir("empty_folder").expect("failed to create temp dir");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}
#[cfg(not(target_os = "windows"))]
#[test]
fn permission_denied_when_deleting() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char '\n')));
    events.push(None);
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(char 'y')));
    events.push(None);
    events.push(Some(key!(Esc)));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path =
        create_root_temp_dir("permission_denied_when_deleting").expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let original_permissions = std::fs::metadata(&subfolder_1_path)
        .expect("failed to read test path metadata")
        .permissions();
    let mut readonly_permissions = original_permissions.clone();
    readonly_permissions.set_readonly(true);
    std::fs::set_permissions(&subfolder_1_path, readonly_permissions)
        .expect("failed to set test path permissions");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "file was not deleted"
    ); // can't really fail on its own, but left here for clarity
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "containing folder was not deleted"
    );

    std::fs::set_permissions(&subfolder_1_path, original_permissions)
        .expect("failed to restore test path permissions");
    drop(temp_dir_path);

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
    assert_snapshot!(&terminal_draw_events_mirror[6]);
    assert_snapshot!(&terminal_draw_events_mirror[7]);
}
#[cfg(not(target_os = "windows"))]
#[test]
fn permission_denied_when_deleting_no_confirmation() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);

    let mut events: Vec<Option<Event>> = std::iter::repeat_n(None, 1).collect();
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(char '\n')));
    events.push(None);
    events.push(None); // the map arms its own cursor; no priming keypress needed
    events.push(None);
    events.push(Some(key!(Backspace)));
    events.push(None);
    events.push(Some(key!(Esc)));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));

    let temp_dir_path = create_root_temp_dir("permission_denied_when_deleting_no_confirmation")
        .expect("failed to create temp dir");

    let mut subfolder_1_path = temp_dir_path.path().to_path_buf();
    subfolder_1_path.push("subfolder1");
    create_dir(&subfolder_1_path).expect("failed to create temporary directory");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("subfolder1");
    file_1_path.push("file1");
    create_temp_file(&file_1_path, 4096).expect("failed to create temp file");

    let original_permissions = std::fs::metadata(&subfolder_1_path)
        .expect("failed to read test path metadata")
        .permissions();
    let mut readonly_permissions = original_permissions.clone();
    readonly_permissions.set_readonly(true);
    std::fs::set_permissions(&subfolder_1_path, readonly_permissions)
        .expect("failed to set test path permissions");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_DISABLED,
    );
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("could not acquire lock on terminal events");

    assert!(
        std::fs::metadata(&file_1_path).is_ok(),
        "file was not deleted"
    ); // can't really fail on its own, but left here for clarity
    assert!(
        std::fs::metadata(&subfolder_1_path).is_ok(),
        "containing folder was not deleted"
    );

    std::fs::set_permissions(&subfolder_1_path, original_permissions)
        .expect("failed to restore test path permissions");
    drop(temp_dir_path);

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("could not acquire lock on terminal_events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
    assert_snapshot!(&terminal_draw_events_mirror[2]);
    assert_snapshot!(&terminal_draw_events_mirror[3]);
    assert_snapshot!(&terminal_draw_events_mirror[4]);
    assert_snapshot!(&terminal_draw_events_mirror[5]);
}

#[test]
fn small_files_with_y_as_zero() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(190, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("small_files_with_y_as_zero").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 1_048_576).expect("failed to create temp file");

    for i in 1..100 {
        let mut small_file_path = temp_dir_path.path().to_path_buf();
        small_file_path.push(format!("small_file{i}"));
        create_temp_file(small_file_path, 4096).expect("failed to create temp file");
    }

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn small_files_with_x_as_zero() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(50, 50);
    let keyboard_events = Box::new(wait_and_quit_events(1, true));
    let temp_dir_path =
        create_root_temp_dir("small_files_with_x_as_zero").expect("failed to create temp dir");

    let mut file_1_path = temp_dir_path.path().to_path_buf();
    file_1_path.push("file1");
    create_temp_file(file_1_path, 1_048_576).expect("failed to create temp file");

    let mut file_2_path = temp_dir_path.path().to_path_buf();
    file_2_path.push("file2");
    create_temp_file(file_2_path, 1_048_576).expect("failed to create temp file");

    for i in 1..100 {
        let mut small_file_path = temp_dir_path.path().to_path_buf();
        small_file_path.push(format!("small_file{i}"));
        create_temp_file(small_file_path, 4096).expect("failed to create temp file");
    }

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    drop(temp_dir_path);
    let terminal_draw_events_mirror = terminal_draw_events
        .lock()
        .expect("failed to lock test state");

    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock test terminal events"),
    );

    assert_snapshot!(&terminal_draw_events_mirror[0]);
    assert_snapshot!(&terminal_draw_events_mirror[1]);
}

#[test]
fn filter_and_help_overlay_are_keyboard_complete() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(100, 30);
    let mut events = vec![None, Some(key!(char '/'))];
    for character in "file2".chars() {
        events.push(Some(key!(char character)));
    }
    events.push(Some(key!(Enter)));
    events.push(None);
    events.push(Some(key!(char '?')));
    events.push(None);
    events.push(Some(key!(Esc)));
    events.push(None);
    events.push(Some(key!(ctrl 'c')));
    events.push(None);
    events.push(Some(key!(char 'y')));
    let keyboard_events = Box::new(TerminalEvents::new(events));
    let temp_dir_path =
        create_root_temp_dir("filter_and_help_overlay").expect("failed to create temp dir");
    create_temp_file(temp_dir_path.path().join("file1"), 4096)
        .expect("first file should be created");
    create_temp_file(temp_dir_path.path().join("file2"), 8192)
        .expect("second file should be created");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events = terminal_draw_events
        .lock()
        .expect("failed to lock test state");
    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock terminal events"),
    );
    assert!(terminal_draw_events.len() >= 3);
    assert_snapshot!(&terminal_draw_events[1]);
    assert_snapshot!(&terminal_draw_events[2]);
}

#[test]
fn theme_change_requires_explicit_save_or_discard_on_exit() {
    let (terminal_events, terminal_draw_events, backend) = test_backend_factory(100, 30);
    let keyboard_events = Box::new(TerminalEvents::new(vec![
        None,
        Some(key!(char 't')),
        None,
        Some(key!(ctrl 'c')),
        None,
        Some(key!(char 'd')),
    ]));
    let temp_dir_path =
        create_root_temp_dir("theme_save_prompt").expect("failed to create temp dir");
    create_temp_file(temp_dir_path.path().join("file"), 4096).expect("fixture should be created");

    start(
        backend,
        keyboard_events,
        temp_dir_path.path().to_path_buf(),
        SHOW_APPARENT_SIZE,
        DELETE_CONFIRMATION_ENABLED,
    );
    let terminal_draw_events = terminal_draw_events
        .lock()
        .expect("failed to lock test state");
    assert_terminal_lifecycle(
        &terminal_events
            .lock()
            .expect("failed to lock terminal events"),
    );
    assert!(terminal_draw_events.len() >= 3);
    assert_snapshot!(&terminal_draw_events[2]);
}
