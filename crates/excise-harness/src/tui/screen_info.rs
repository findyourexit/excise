//! The documents' views of a running program: its screen, its events, and its exit.

use std::path::Path;

use crate::{
    events::{Event, Payload, RefreshOutcome},
    pty::{
        BoxRect, BoxView, PtySession, Screen, TerminalModes,
        ui::{
            Confirmation, DeleteDialog, DeleteKind, DialogView, Inspector, dialog_view,
            filter_prompt, header_path, header_state, inspector,
        },
    },
    report::tui::{
        BoxInfo, ConfirmationKind, Cursor, DeleteDialogInfo, DeleteDialogKind, DialogInfo,
        EventRecord, EventRecordKind, EventsPage, ExitInfo, ExitVia, FilterInfo, Modes, Rect,
        RefreshOutcomeKind, ScreenInfo, SelectedInfo, Size,
    },
};

/// What the screen model shows now.
pub(super) fn screen_info(session: &PtySession) -> ScreenInfo {
    let screen = session.screen();
    let (rows, cols) = screen.size();
    let (cursor_row, cursor_col) = screen.cursor();
    ScreenInfo {
        size: Size { cols, rows },
        cursor: Cursor {
            row: cursor_row,
            col: cursor_col,
        },
        modes: modes_info(session.modes()),
        header_state: header_state(screen).map(|state| state.label().to_owned()),
        selected: match inspector(screen) {
            Inspector::Item(item) => Some(SelectedInfo {
                name: item.name,
                state: item.state,
                kind: item.kind,
            }),
            Inspector::NotShown | Inspector::NothingSelected => None,
        },
        filter: filter_prompt(screen).map(|prompt| FilterInfo {
            input: prompt.input,
            error: prompt.error,
        }),
        dialog: dialog_info(screen),
        boxes: screen
            .boxes()
            .into_iter()
            .map(|view| BoxInfo {
                title: view.title,
                rect: rect_info(view.rect),
            })
            .collect(),
        rows: (0..rows).map(|row| screen.row_text(row)).collect(),
    }
}

/// The terminal modes.
pub(super) const fn modes_info(modes: TerminalModes) -> Modes {
    Modes {
        alternate_screen: modes.alternate_screen,
        cursor_visible: modes.cursor_visible,
        echo: modes.echo,
        icanon: modes.icanon,
    }
}

const fn rect_info(rect: BoxRect) -> Rect {
    Rect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn dialog_info(screen: &Screen) -> Option<DialogInfo> {
    match dialog_view(screen) {
        DialogView::None => None,
        DialogView::Other(view) => Some(dialog_from(&view, None)),
        DialogView::Delete(dialog) => Some(delete_dialog_info(&dialog)),
    }
}

fn dialog_from(view: &BoxView, delete: Option<DeleteDialogInfo>) -> DialogInfo {
    DialogInfo {
        title: view.title.clone(),
        rect: rect_info(view.rect),
        text: view.text.clone(),
        delete,
    }
}

/// The document's view of a deletion dialog.
pub(super) fn delete_dialog_info(dialog: &DeleteDialog) -> DialogInfo {
    dialog_from(
        &dialog.view,
        Some(DeleteDialogInfo {
            kind: match dialog.kind {
                DeleteKind::Folder => DeleteDialogKind::Folder,
                DeleteKind::File => DeleteDialogKind::File,
                DeleteKind::Item => DeleteDialogKind::Item,
            },
            path: dialog.path.clone(),
            confirmation: match &dialog.confirmation {
                Confirmation::SingleKey => ConfirmationKind::SingleKey,
                Confirmation::TypedPhrase(_) => ConfirmationKind::TypedPhrase,
                Confirmation::Unknown => ConfirmationKind::Unknown,
            },
        }),
    )
}

/// The records of `events` from index `since` on, and the index after the last one.
///
/// A `since` past the end covers no records; the page says where it really starts.
pub(super) fn events_page(events: &[Event], since: u64) -> EventsPage {
    let start = usize::try_from(since).map_or(events.len(), |since| since.min(events.len()));
    EventsPage {
        since: start as u64,
        next: events.len() as u64,
        records: events[start..]
            .iter()
            .enumerate()
            .map(|(offset, event)| record(start + offset, event))
            .collect(),
    }
}

fn record(index: usize, event: &Event) -> EventRecord {
    let mut record = EventRecord {
        index: index as u64,
        t_us: event.t_us,
        kind: EventRecordKind::Hello,
        version: None,
        pid: None,
        frame_marks: None,
        input_barrier: None,
        seq: None,
        inputs: None,
        barriers: None,
        entries: None,
        removed: None,
        failed: None,
        outcome: None,
        code: None,
    };
    match &event.payload {
        Payload::Hello {
            version,
            pid,
            frame_marks,
            input_barrier,
        } => {
            record.version = Some(version.clone());
            record.pid = Some(*pid);
            record.frame_marks = Some(*frame_marks);
            record.input_barrier = Some(*input_barrier);
        }
        Payload::Frame {
            seq,
            inputs,
            barriers,
        } => {
            record.kind = EventRecordKind::Frame;
            record.seq = Some(*seq);
            record.inputs = Some(*inputs);
            record.barriers = Some(*barriers);
        }
        Payload::ScanComplete { entries } => {
            record.kind = EventRecordKind::ScanComplete;
            record.entries = Some(*entries);
        }
        Payload::QuitPrompt => record.kind = EventRecordKind::QuitPrompt,
        Payload::DeletionFinished { removed, failed } => {
            record.kind = EventRecordKind::DeletionFinished;
            record.removed = Some(*removed);
            record.failed = Some(*failed);
        }
        Payload::RefreshFinished { outcome } => {
            record.kind = EventRecordKind::RefreshFinished;
            record.outcome = Some(match outcome {
                RefreshOutcome::Published => RefreshOutcomeKind::Published,
                RefreshOutcome::Failed => RefreshOutcomeKind::Failed,
            });
        }
        Payload::Exit { code } => {
            record.kind = EventRecordKind::Exit;
            record.code = Some(*code);
        }
    }
    record
}

/// How the program ended, if it has: its status, how it came to end, and the terminal it left.
pub(super) fn exit_info(session: &PtySession, via: ExitVia) -> Option<ExitInfo> {
    let exit = session.exit()?;
    let modes = session.modes();
    Some(ExitInfo {
        code: exit.code,
        signal: exit.signal,
        via,
        terminal_restored: modes.restored(),
        modes: modes_info(modes),
    })
}

/// The folder the program shows, relative to the fixture's `root` (empty for the root itself), as
/// the header says it.
///
/// `None` when the header cannot say: it is not drawn yet, or the path is cut (the program cuts a
/// path that does not fit), or it is not a path below `root`.
pub(super) fn open_folder(screen: &Screen, root: &Path) -> Option<String> {
    let shown = header_path(screen)?;
    if is_cut(&shown) {
        return None;
    }
    let root = root.to_str()?.trim_end_matches('/');
    let below = shown.strip_prefix(root)?;
    let folder = if below.is_empty() {
        ""
    } else {
        below.strip_prefix('/')?
    };
    Some(folder.trim_end_matches('/').to_owned())
}

/// Whether the program cut `path` to fit the header: it replaces the middle of a path that does
/// not fit with `[...]` or `[..]`, so what is left need not be the path of any folder. A folder
/// whose own name holds that text counts as cut too, which only sends the caller to the
/// verified protocol, as an unreadable header does.
fn is_cut(path: &str) -> bool {
    path.contains("[..")
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::events::parse_line;

    fn event(index: usize, line: &str) -> Event {
        parse_line(index + 1, line.as_bytes(), Instant::now()).expect("an event")
    }

    fn sample() -> Vec<Event> {
        [
            r#"{"v":1,"kind":"hello","version":"1.3.0","pid":42,"frame_marks":true,"input_barrier":true,"t_us":0}"#,
            r#"{"v":1,"kind":"frame","seq":1,"inputs":0,"barriers":2,"t_us":10}"#,
            r#"{"v":1,"kind":"scan_complete","entries":9,"t_us":20}"#,
            r#"{"v":1,"kind":"quit_prompt","t_us":30}"#,
            r#"{"v":1,"kind":"deletion_finished","removed":5,"failed":1,"t_us":40}"#,
            r#"{"v":1,"kind":"exit","code":0,"t_us":50}"#,
        ]
        .iter()
        .enumerate()
        .map(|(index, line)| event(index, line))
        .collect()
    }

    #[test]
    fn every_kind_of_event_becomes_a_record_that_carries_its_fields_only() {
        let page = events_page(&sample(), 0);

        assert_eq!((page.since, page.next), (0, 6));
        let kinds: Vec<_> = page.records.iter().map(|record| record.kind).collect();
        assert_eq!(
            kinds,
            [
                EventRecordKind::Hello,
                EventRecordKind::Frame,
                EventRecordKind::ScanComplete,
                EventRecordKind::QuitPrompt,
                EventRecordKind::DeletionFinished,
                EventRecordKind::Exit,
            ]
        );
        let json = serde_json::to_value(&page.records).expect("records");
        assert_eq!(
            json[0],
            serde_json::json!({"index":0,"t_us":0,"kind":"hello","version":"1.3.0","pid":42,"frame_marks":true,"input_barrier":true})
        );
        assert_eq!(
            json[1],
            serde_json::json!({"index":1,"t_us":10,"kind":"frame","seq":1,"inputs":0,"barriers":2})
        );
        assert_eq!(
            json[2],
            serde_json::json!({"index":2,"t_us":20,"kind":"scan_complete","entries":9})
        );
        assert_eq!(
            json[3],
            serde_json::json!({"index":3,"t_us":30,"kind":"quit_prompt"})
        );
        assert_eq!(
            json[4],
            serde_json::json!({"index":4,"t_us":40,"kind":"deletion_finished","removed":5,"failed":1})
        );
        assert_eq!(
            json[5],
            serde_json::json!({"index":5,"t_us":50,"kind":"exit","code":0})
        );
    }

    #[test]
    fn a_page_starts_where_it_is_asked_to_and_says_where_to_go_on() {
        let events = sample();

        let middle = events_page(&events, 4);
        assert_eq!((middle.since, middle.next, middle.records.len()), (4, 6, 2));
        assert_eq!(middle.records[0].index, 4);

        let past = events_page(&events, 99);
        assert_eq!((past.since, past.next, past.records.len()), (6, 6, 0));

        let empty = events_page(&[], 0);
        assert_eq!((empty.since, empty.next, empty.records.len()), (0, 0, 0));
        assert_eq!(events_page(&events, u64::MAX).since, 6);
    }

    fn header(row: &str) -> Screen {
        let mut screen = Screen::new(5, 100);
        screen.process(format!("\u{1b}[1;1H{row}").as_bytes());
        screen
    }

    #[test]
    fn the_open_folder_is_the_header_path_below_the_root() {
        let root = Path::new("/tmp/xh-tui-0123abcd/delete-file-7-0");
        for (row, folder) in [
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0/  ◆ COMPLETE",
                Some(""),
            ),
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0  ◆ COMPLETE",
                Some(""),
            ),
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0/docs  ◆ COMPLETE",
                Some("docs"),
            ),
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0/docs/deep/  ◌ SCANNING",
                Some("docs/deep"),
            ),
            // A folder that merely starts like the root, a path cut in the middle the way the
            // program cuts it, after the whole root or inside it, and no header at all.
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0-2/docs  ◆ COMPLETE",
                None,
            ),
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0/docs/[...]/deep/leaf  ◆ COMPLETE",
                None,
            ),
            (
                " EXCISE  /tmp/xh-tui-0123abcd/delete-file-7-0/docs/[..]/leaf  ◆ COMPLETE",
                None,
            ),
            (
                " EXCISE  /tmp/xh-tui-01[..]ete-file-7-0/docs  ◆ COMPLETE",
                None,
            ),
            ("Loading...", None),
        ] {
            assert_eq!(
                open_folder(&header(row), root).as_deref(),
                folder,
                "{row:?}"
            );
        }
    }

    fn frame_line(number: usize) -> String {
        format!(
            r#"{{"v":1,"kind":"frame","seq":{},"inputs":{number},"t_us":{}}}"#,
            number + 1,
            100 + 50 * number
        )
    }

    fn events_of(lines: &[String]) -> Vec<Event> {
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| event(index, line))
            .collect()
    }

    #[test]
    fn frames_are_summarized_and_every_other_record_is_kept_in_full() {
        let events = events_of(&[
            r#"{"v":1,"kind":"hello","version":"1.3.0","pid":42,"t_us":0}"#.to_owned(),
            frame_line(0),
            frame_line(1),
            frame_line(2),
            r#"{"v":1,"kind":"scan_complete","entries":9,"t_us":400}"#.to_owned(),
            frame_line(3),
            r#"{"v":1,"kind":"exit","code":0,"t_us":900}"#.to_owned(),
        ]);

        let digest = events_page(&events, 0).digest();

        assert_eq!((digest.since, digest.next), (0, 7));
        let frames = digest.frames.expect("frames");
        assert_eq!(
            (frames.count, frames.first_t_us, frames.last_t_us),
            (4, 100, 250)
        );
        assert_eq!(
            (
                frames.last.index,
                frames.last.kind,
                frames.last.seq,
                frames.last.t_us
            ),
            (5, EventRecordKind::Frame, Some(4), 250)
        );
        let kept: Vec<_> = digest
            .records
            .iter()
            .map(|record| (record.index, record.kind))
            .collect();
        assert_eq!(
            kept,
            [
                (0, EventRecordKind::Hello),
                (4, EventRecordKind::ScanComplete),
                (6, EventRecordKind::Exit),
            ]
        );
        assert_eq!(digest.records[1].entries, Some(9));

        // Nothing but frames, and no frames at all.
        let only_frames = events_page(&events[1..4], 0).digest();
        assert!(only_frames.records.is_empty());
        assert_eq!(only_frames.frames.expect("frames").count, 3);
        let no_frames = events_page(&events[..1], 0).digest();
        assert!(no_frames.frames.is_none());
        assert_eq!(no_frames.records.len(), 1);
        assert!(events_page(&[], 0).digest().frames.is_none());
    }

    #[test]
    fn a_long_idle_between_commands_makes_a_digest_no_larger() {
        let mut lines =
            vec![r#"{"v":1,"kind":"hello","version":"1.3.0","pid":42,"t_us":0}"#.to_owned()];
        lines.extend((0..20_000).map(frame_line));
        lines.push(r#"{"v":1,"kind":"scan_complete","entries":9,"t_us":2000000}"#.to_owned());
        let events = events_of(&lines);

        let page = events_page(&events, 0);
        let digest = page.clone().digest();

        assert_eq!(page.records.len(), 20_002);
        assert_eq!(
            digest.frames.as_ref().map(|frames| frames.count),
            Some(20_000)
        );
        assert_eq!(digest.records.len(), 2);
        let size = serde_json::to_string(&digest).expect("a digest").len();
        assert!(size < 700, "{size} bytes");
    }
}
