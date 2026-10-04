//! Everything that runs the real runtime on one input: the backend that records what the
//! terminal was told, the input source that plays the script, the watchdog, and the checks that
//! need only how the run ended.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crossterm::event::Event;
use excise::fuzz::error::AppError;
use excise::fuzz::input::{InputEvent, InputSource};
use excise::fuzz::runtime::{RuntimeSettings, VirtualClock, run};
use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Size};

use crate::model::{Model, Stats};
use crate::script::{self, FILTER_PATTERNS, Key, Op, SIZES, Script, Step, select};
use crate::world::World;

/// The most one input may take, wall clock, counted from the moment `run_input` starts. Past it
/// the run is a hang. The clock does not start again however many steps the script has: a run
/// takes a few hundred milliseconds.
const INPUT_BUDGET: Duration = Duration::from_secs(45);
/// How long the runtime may go on after the script has run out of keys.
const EXHAUSTED_GRACE: Duration = Duration::from_secs(3);
const EXHAUSTED: &str = "the script ran out of keys while the runtime was still running";

// --- the terminal ------------------------------------------------------------------------------

/// What the runtime asked of the terminal that matters to its restoration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalEvent {
    Clear,
    HideCursor,
    ShowCursor,
    Draw,
    Flush,
}

/// A terminal that lives in memory, records the calls that frame the session, and can be
/// resized. It also tells the model whether a deletion dialog is on screen.
pub struct RecordingBackend {
    inner: TestBackend,
    events: Arc<Mutex<Vec<TerminalEvent>>>,
    size: Arc<Mutex<(u16, u16)>>,
    screen: Arc<Mutex<String>>,
    model: Option<Arc<Model>>,
}

/// What the run and the backend share.
#[derive(Clone)]
pub struct TerminalHandles {
    pub events: Arc<Mutex<Vec<TerminalEvent>>>,
    pub size: Arc<Mutex<(u16, u16)>>,
    pub screen: Arc<Mutex<String>>,
}

impl RecordingBackend {
    pub fn new(columns: u16, rows: u16) -> (Self, TerminalHandles) {
        let handles = TerminalHandles {
            events: Arc::new(Mutex::new(Vec::new())),
            size: Arc::new(Mutex::new((columns, rows))),
            screen: Arc::new(Mutex::new(String::new())),
        };
        let backend = Self {
            inner: TestBackend::new(columns, rows),
            events: Arc::clone(&handles.events),
            size: Arc::clone(&handles.size),
            screen: Arc::clone(&handles.screen),
            model: None,
        };
        (backend, handles)
    }

    pub fn watched_by(mut self, model: Arc<Model>) -> Self {
        self.model = Some(model);
        self
    }

    fn record(&self, event: TerminalEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    /// Makes the buffer behind the backend the size the script last asked for.
    fn follow_size(&mut self) {
        let (columns, rows) = *self.size.lock().unwrap_or_else(PoisonError::into_inner);
        let area = self.inner.buffer().area;
        if (area.width, area.height) != (columns, rows) {
            self.inner.resize(columns, rows);
        }
    }

    fn look_at_the_screen(&mut self) {
        let text = screen_text(self.inner.buffer());
        if let Some(model) = &self.model {
            model.frame(dialog_in(&text).as_deref());
        }
        *self.screen.lock().unwrap_or_else(PoisonError::into_inner) = text;
    }
}

fn screen_text(buffer: &Buffer) -> String {
    let width = usize::from(buffer.area.width).max(1);
    let mut text = String::with_capacity(buffer.content().len() + buffer.content().len() / width);
    for row in buffer.content().chunks(width) {
        for cell in row {
            text.push_str(cell.symbol());
        }
        text.truncate(text.trim_end_matches(' ').len());
        text.push('\n');
    }
    text
}

/// What the deletion dialog on `screen` says of itself, its title, or `None` when no such dialog
/// is open. The words that hold no letter or digit are the dialog's frame. The path the dialog
/// names is left out: the screen cuts it to its width, and the trace must read the same whichever
/// directory a run built its fixture in.
fn dialog_in(screen: &str) -> Option<String> {
    let title = screen.lines().find(|row| row.contains("! DELETE "))?;
    Some(
        title
            .split_whitespace()
            .filter(|word| word.chars().any(char::is_alphanumeric))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

impl Backend for RecordingBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.follow_size();
        self.record(TerminalEvent::Draw);
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.record(TerminalEvent::HideCursor);
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.record(TerminalEvent::ShowCursor);
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.follow_size();
        self.record(TerminalEvent::Clear);
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.follow_size();
        if clear_type == ClearType::All {
            self.record(TerminalEvent::Clear);
        }
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        let (columns, rows) = *self.size.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(Size::new(columns, rows))
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.follow_size();
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.record(TerminalEvent::Flush);
        self.look_at_the_screen();
        self.inner.flush()
    }
}

/// The terminal's session is framed as the runtime must leave it: the screen cleared and the
/// cursor hidden first, then whole frames (each drawn, the cursor hidden, flushed; a resize
/// clears the screen before the next one), and last the screen cleared and the cursor shown
/// again, with nothing after. Anything else leaves the user's terminal in the runtime's state.
fn check_terminal_restored(events: &[TerminalEvent]) -> Result<(), String> {
    use TerminalEvent::{Clear, Draw, Flush, HideCursor, ShowCursor};

    let fail = |why: &str| Err(format!("{why}; the terminal was told: {events:?}"));
    if events.len() < 7 {
        return fail("the session was too short to have entered and left the terminal");
    }
    if events[..2] != [Clear, HideCursor] {
        return fail("the session did not begin by clearing the screen and hiding the cursor");
    }
    if events[events.len() - 2..] != [Clear, ShowCursor] {
        return fail("the session did not end by clearing the screen and showing the cursor");
    }
    let mut frames = &events[2..events.len() - 2];
    let mut drawn = false;
    loop {
        while let [Clear, rest @ ..] = frames {
            frames = rest;
        }
        match frames {
            [] => break,
            [Draw, HideCursor, Flush, rest @ ..] => {
                drawn = true;
                frames = rest;
            }
            _ => {
                return fail(
                    "the session's calls between entering and leaving are not whole frames",
                );
            }
        }
    }
    if !drawn {
        return fail("the session drew no frame");
    }
    Ok(())
}

// --- the script --------------------------------------------------------------------------------

enum What {
    Key(Key),
    Resize(u16, u16),
    Barrier,
    Mutate(Op),
    Arm(Op),
}

struct Action {
    step: usize,
    text: Arc<str>,
    what: What,
}

/// Expands the script into the actions the runtime is fed: the initial settle in front, each
/// key, filter, or resize step behind a settle (unless a `^` before it took that settle away),
/// and the closing tail after. A step that settles itself (`.`, `m`, `A`) gets no second one.
fn expand(script: &Script) -> VecDeque<Action> {
    let mut actions = VecDeque::new();
    let mut push = |step: usize, text: &Arc<str>, what: What| {
        actions.push_back(Action {
            step,
            text: Arc::clone(text),
            what,
        });
    };
    let start: Arc<str> = Arc::from("settle after the scan");
    push(0, &start, What::Barrier);
    let mut race = false;
    for (number, step) in script.steps.iter().chain(script::TAIL.iter()).enumerate() {
        let number = number + 1;
        let text: Arc<str> = Arc::from(step.to_string());
        if matches!(step, Step::Race) {
            race = true;
            continue;
        }
        let raced = std::mem::take(&mut race);
        if !raced && !matches!(step, Step::Settle | Step::Mutate(_) | Step::Arm(_)) {
            push(number, &text, What::Barrier);
        }
        match *step {
            Step::Key(key) => push(number, &text, What::Key(key)),
            Step::Filter { pattern, commit } => {
                push(number, &text, What::Key(Key::Char('/')));
                for character in FILTER_PATTERNS[select(pattern, FILTER_PATTERNS.len())].chars() {
                    push(number, &text, What::Key(Key::Char(character)));
                }
                push(
                    number,
                    &text,
                    What::Key(if commit { Key::Enter } else { Key::Esc }),
                );
            }
            Step::Resize(size) => {
                let (columns, rows) = SIZES[select(size, SIZES.len())];
                push(number, &text, What::Resize(columns, rows));
            }
            Step::Settle => push(number, &text, What::Barrier),
            Step::Mutate(op) => {
                push(number, &text, What::Barrier);
                push(number, &text, What::Mutate(op));
            }
            Step::Arm(op) => {
                push(number, &text, What::Barrier);
                push(number, &text, What::Arm(op));
            }
            Step::Race => {}
        }
    }
    actions
}

/// Plays the script. Mutations and arming run inside `read`, right after the barrier that comes
/// before them has returned, when the runtime is idle.
struct ScriptedInput {
    actions: VecDeque<Action>,
    model: Arc<Model>,
    handles: TerminalHandles,
    out_of_keys_since: Option<Instant>,
}

impl InputSource for ScriptedInput {
    fn poll(&mut self, timeout: Duration) -> Result<bool, AppError> {
        if !self.actions.is_empty() {
            return Ok(true);
        }
        // The script is over. The runtime may still be finishing what the last key started,
        // but it must quit by itself, and soon.
        let since = *self.out_of_keys_since.get_or_insert_with(Instant::now);
        if since.elapsed() > EXHAUSTED_GRACE {
            return Err(AppError::Invariant(EXHAUSTED.to_owned()));
        }
        std::thread::sleep(timeout.min(Duration::from_millis(5)));
        Ok(false)
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        // The runtime asks for more: it is done with the key it had.
        self.model.key_handled();
        loop {
            let Some(action) = self.actions.pop_front() else {
                return Err(AppError::Invariant(
                    "fuzz input exhausted after poll".to_owned(),
                ));
            };
            self.model.step(action.step, &action.text);
            match action.what {
                What::Key(key) => {
                    self.model.key(key);
                    return Ok(InputEvent::Terminal(Event::Key(key.event())));
                }
                What::Resize(columns, rows) => {
                    *self
                        .handles
                        .size
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = (columns, rows);
                    return Ok(InputEvent::Terminal(Event::Resize(columns, rows)));
                }
                What::Barrier => return Ok(InputEvent::Barrier),
                What::Mutate(op) => self.model.mutate_when_idle(op),
                What::Arm(op) => self.model.arm(op),
            }
        }
    }
}

// --- the watchdog ------------------------------------------------------------------------------

struct Watched {
    deadline: Instant,
    model: Arc<Model>,
}

static WATCHED: Mutex<Option<Watched>> = Mutex::new(None);

/// The thread that turns a hang into a crash with a message. libFuzzer's own timeout is twenty
/// minutes, and it says nothing of where the run was. It starts when first forced.
static WATCHDOG: LazyLock<()> = LazyLock::new(|| {
    std::thread::Builder::new()
        .name("deletion-lifecycle-watchdog".to_owned())
        .spawn(|| {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let guard = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(watched) = guard.as_ref()
                    && Instant::now() > watched.deadline
                {
                    eprintln!(
                        "deletion safety violated: the interface did not return to a \
                         navigable state: the run took longer than {INPUT_BUDGET:?}, \
                         stuck at {}\n{}",
                        watched.model.current_step_unlocked(),
                        watched.model.describe_unlocked(),
                    );
                    std::process::abort();
                }
            }
        })
        .expect("the watchdog thread should start");
});

// --- the statistics ----------------------------------------------------------------------------

/// What all the inputs of this process reached, printed now and then.
#[derive(Default)]
struct Totals {
    inputs: AtomicU64,
    shown: AtomicU64,
    confirmed: AtomicU64,
    deleted: AtomicU64,
    cancelled: AtomicU64,
    mutated: AtomicU64,
    mutated_with_dialog: AtomicU64,
    confirmed_after_mutation: AtomicU64,
    armed: AtomicU64,
    refused: AtomicU64,
    entries_deleted: AtomicU64,
    windows: AtomicU64,
}

impl Totals {
    fn add(&self, stats: &Stats) {
        let count = |total: &AtomicU64, reached: bool| {
            total.fetch_add(u64::from(reached), Ordering::Relaxed);
        };
        let inputs = self.inputs.fetch_add(1, Ordering::Relaxed) + 1;
        count(&self.shown, stats.dialogs_shown > 0);
        count(&self.confirmed, stats.executions > 0);
        count(&self.deleted, stats.deleted_entries > 0);
        count(
            &self.cancelled,
            stats.cancels_on_dialog > 0 || stats.soft_cancelled > 0,
        );
        count(&self.mutated, stats.mutations > 0);
        count(&self.mutated_with_dialog, stats.mutations_with_dialog > 0);
        count(
            &self.confirmed_after_mutation,
            stats.confirms_after_mutation > 0,
        );
        count(&self.armed, stats.armed_mutations > 0);
        count(&self.refused, stats.refused > 0);
        self.entries_deleted
            .fetch_add(stats.deleted_entries, Ordering::Relaxed);
        self.windows.fetch_add(stats.windows, Ordering::Relaxed);
        if inputs.is_multiple_of(100) {
            self.print();
        }
    }

    fn print(&self) {
        let get = |total: &AtomicU64| total.load(Ordering::Relaxed);
        eprintln!(
            "deletion_lifecycle: {} inputs; reached: dialog {}, confirmed deletion {}, \
             entries deleted {} (in {} inputs), cancelled {}, mutated {}, mutated with the \
             dialog open {} (then confirmed {}), mutated between plan and final check {}, \
             refused by the final check {}; {} windows checked",
            get(&self.inputs),
            get(&self.shown),
            get(&self.confirmed),
            get(&self.entries_deleted),
            get(&self.deleted),
            get(&self.cancelled),
            get(&self.mutated),
            get(&self.mutated_with_dialog),
            get(&self.confirmed_after_mutation),
            get(&self.armed),
            get(&self.refused),
            get(&self.windows),
        );
    }
}

static TOTALS: LazyLock<Totals> = LazyLock::new(Totals::default);

// --- one input ---------------------------------------------------------------------------------

fn settings(world: &World, script: &Script) -> RuntimeSettings {
    let metadata = std::fs::symlink_metadata(&world.root).expect("the fixture root has metadata");
    let root_identity = excise::fuzz::native_path::identity_for(&world.root, &metadata)
        .expect("the fixture root has an identity")
        .expect("the fixture root is not a symbolic link");
    RuntimeSettings {
        root: world.root.clone(),
        root_identity,
        scan_threads: 1,
        event_capacity: 16,
        cross_filesystems: false,
        exclusions: Vec::new(),
        memory_mib: excise::fuzz::model::DEFAULT_PROCESS_MIB,
        temporary_storage_mib: 2,
        scan_store_mib: Some(2),
        scan_store_reserve_mib: None,
        scan_store_dir: Some(world.store.clone()),
        apparent_size: !script.shape.allocated_sizes,
        disable_delete_confirmation: script.shape.reduced_confirmation,
        reduced_motion: !script.shape.full_motion,
        theme: excise::fuzz::theme::ThemeId::ExciseDark,
        ascii: false,
        mouse: false,
        keymap: excise::fuzz::config::KeyPreset::Vim,
        custom_keys: None,
        monochrome: true,
        animate_loading: false,
        config_path: None,
        monochrome_locked: true,
    }
}

/// Restores the working directory when dropped.
struct WorkingDirectory(PathBuf);

impl WorkingDirectory {
    fn enter(directory: &std::path::Path) -> Self {
        let previous = std::env::current_dir().expect("the working directory is readable");
        std::env::set_current_dir(directory).expect("the run's working directory exists");
        Self(previous)
    }
}

impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// A number that stands for the bytes of an input, so that the replays of one input can be told
/// from those of another.
fn fingerprint(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Runs the real runtime on the fixture and the script `data` decodes to, and panics, with a
/// message that names the step and the paths, when a check fails.
pub fn run_input(data: &[u8]) {
    let started = Instant::now();
    LazyLock::force(&WATCHDOG);
    let script = script::decode(data);
    let private = tempfile::Builder::new()
        .prefix("excise-fuzz-deletion-lifecycle-")
        .tempdir()
        .expect("a private directory for the fixture");
    let base = std::fs::canonicalize(private.path()).expect("the private directory resolves");
    let world = World::build(&base, script.shape).expect("the fixture should build");

    let (backend, handles) = RecordingBackend::new(80, 24);
    let model = Model::new(world.clone(), Arc::clone(&handles.screen))
        .expect("the fixture should have a snapshot");
    let backend = backend.watched_by(Arc::clone(&model));
    let input = ScriptedInput {
        actions: expand(&script),
        model: Arc::clone(&model),
        handles: handles.clone(),
        out_of_keys_since: None,
    };
    let observed = Arc::clone(&model);
    let probe = excise::fuzz::deletion::install_probe(observed);
    let working_directory = WorkingDirectory::enter(&world.cwd);

    *WATCHED.lock().unwrap_or_else(PoisonError::into_inner) = Some(Watched {
        deadline: started + INPUT_BUDGET,
        model: Arc::clone(&model),
    });
    let run_started = Instant::now();
    let result = run(
        backend,
        Box::new(input),
        settings(&world, &script),
        Box::new(VirtualClock::new()),
    );
    let run_time = run_started.elapsed();
    *WATCHED.lock().unwrap_or_else(PoisonError::into_inner) = None;
    drop(working_directory);
    drop(probe);

    // 4. The terminal is restored, whatever way the run ended.
    let events = handles
        .events
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Err(why) = check_terminal_restored(&events) {
        panic!(
            "deletion safety violated: the terminal is restored\n{why}\nat {}\n{}",
            model.current_step(),
            model.describe()
        );
    }

    // 3. The interface returns to a navigable state: the scripted quit ends the run.
    match result {
        Ok(outcome) => model.note(&format!("the run ended: {outcome:?}")),
        Err(AppError::Invariant(message)) if message == EXHAUSTED => panic!(
            "deletion safety violated: the interface returns to a navigable state\n\
             {message}: the closing keys (Esc four times, then q, c, s and y) did not end the \
             run\nat {}\n{}",
            model.current_step(),
            model.describe()
        ),
        Err(error) => panic!(
            "the runtime failed instead of ending through the scripted quit: {error}\nat {}\n{}",
            model.current_step(),
            model.describe()
        ),
    }

    // 1 and 2. What the runtime changed in the last window was reviewed and confirmed.
    model.finish();
    let stats = model.stats();
    TOTALS.add(&stats);
    drop(private);
    if std::env::var_os("EXCISE_FUZZ_TRACE").is_some() {
        eprintln!(
            "{}\nreached: {stats:?}\nreplay input {:016x} trace {:016x}\ntook {:?}, of which the \
             runtime {run_time:?}",
            model.describe(),
            fingerprint(data),
            model.digest(),
            started.elapsed()
        );
    }
}
