//! Reading a spec file. A directory of specs of one's own (`cargo xtask headless --fixture-dir`,
//! `cargo xtask bench-e2e --fixture-dir`) is not trusted to hold only specs: a spec is opened
//! without following a link, must be a regular file, and is read up to a cap, whoever asks, so that
//! a link to something that is not a spec, a FIFO that nobody writes to, a folder, and a file of
//! gigabytes are each refused with a message, and none is waited on or read in full.

use std::fs;
#[cfg(unix)]
use std::{path::Path, sync::mpsc, thread, time::Duration};

use super::support::Scratch;
#[cfg(unix)]
use crate::fixture::FixtureError;
use crate::fixture::{FixtureCache, FixtureSpec, Fixtures, SpecError, spec::MAX_SPEC_BYTES};

/// The text of a spec with the id `id`.
fn spec_text(id: &str) -> String {
    format!(
        "schema_version = 1\nid = \"{id}\"\ndescription = \"A spec.\"\nseed = 1\n\n\
         [[parts]]\nkind = \"file\"\nroot = \"f.bin\"\nsize = 8\n"
    )
}

/// A directory of specs in `scratch`, holding the spec `plain`.
fn directory_of_specs(scratch: &Scratch) -> std::path::PathBuf {
    let dir = scratch.join("specs");
    fs::create_dir(&dir).expect("a directory of specs");
    fs::write(dir.join("plain.toml"), spec_text("plain")).expect("a spec");
    dir
}

/// The text of the error that loading the spec `id` of `dir` gives, which must be one that is
/// about reading.
fn refusal(dir: &std::path::Path, id: &str) -> String {
    let error = FixtureSpec::load(dir, id).expect_err("the spec is refused");
    assert!(
        matches!(error, SpecError::Read { .. }),
        "a refusal to read, not a parse error: {error}"
    );
    error.to_string()
}

#[test]
fn a_spec_that_is_a_regular_file_is_loaded_by_every_way_in() {
    let scratch = Scratch::new();
    let dir = directory_of_specs(&scratch);

    let spec = FixtureSpec::load(&dir, "plain").expect("a spec");
    assert_eq!(spec.id, "plain");
    let fixtures = Fixtures::new(&dir, FixtureCache::at(scratch.join("cache")));
    assert_eq!(fixtures.spec("plain").expect("a spec"), spec);
    assert_eq!(fixtures.ids().expect("ids"), ["plain"]);
}

#[cfg(unix)]
#[test]
fn a_spec_that_is_a_symbolic_link_is_refused_and_not_followed() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let dir = directory_of_specs(&scratch);
    // A link to a spec that is the spec of its own name, as far as its text goes, in the
    // directory and outside it, and a link to nothing.
    fs::write(dir.join("backing.toml"), spec_text("linked")).expect("a spec");
    symlink("backing.toml", dir.join("linked.toml")).expect("a link");
    fs::write(scratch.join("elsewhere.toml"), spec_text("away")).expect("a spec");
    symlink(scratch.join("elsewhere.toml"), dir.join("away.toml")).expect("a link");
    symlink("nowhere.toml", dir.join("nothing.toml")).expect("a link");

    for id in ["linked", "away", "nothing"] {
        let text = refusal(&dir, id);
        assert!(text.contains("symbolic link"), "{id}: {text}");
        assert!(text.contains(&format!("{id}.toml")), "{id}: {text}");
    }
    // The same through the facade the runners use: the spec, the cached master that `headless`
    // and `bench-e2e` scan, and the run copy, none of which generates anything from a link.
    let fixtures = Fixtures::new(&dir, FixtureCache::at(scratch.join("cache")));
    assert!(
        matches!(fixtures.spec("linked"), Err(SpecError::Read { .. })),
        "a runner is refused too"
    );
    assert!(matches!(
        fixtures.master("linked"),
        Err(FixtureError::Spec(SpecError::Read { .. }))
    ));
    fs::create_dir(scratch.join("runs")).expect("a directory for run copies");
    assert!(matches!(
        fixtures.run_copy("linked", &scratch.join("runs")),
        Err(FixtureError::Spec(SpecError::Read { .. }))
    ));
    assert!(
        !scratch.join("cache").exists(),
        "nothing was generated in the cache"
    );
}

#[test]
fn a_spec_that_is_a_folder_is_refused_as_not_a_regular_file() {
    let scratch = Scratch::new();
    let dir = directory_of_specs(&scratch);
    fs::create_dir(dir.join("folder.toml")).expect("a folder");

    let text = refusal(&dir, "folder");
    assert!(text.contains("not a regular file"), "{text}");
}

/// `FixtureSpec::load`, on a thread that has `limit` to finish. A load that waits is a failure of
/// this test and not a test run that never ends: the FIFO is opened for writing, which lets a
/// reader that waits on it go on, and then the test fails.
#[cfg(unix)]
fn load_within(
    dir: &Path,
    id: &str,
    limit: Duration,
    fifo: &Path,
) -> Result<FixtureSpec, SpecError> {
    let (sender, receiver) = mpsc::channel();
    let (dir, id) = (dir.to_path_buf(), id.to_owned());
    thread::spawn(move || {
        let _ = sender.send(FixtureSpec::load(&dir, &id));
    });
    if let Ok(result) = receiver.recv_timeout(limit) {
        return result;
    }
    let writer = fs::OpenOptions::new().read(true).write(true).open(fifo);
    thread::sleep(Duration::from_millis(200));
    drop(writer);
    panic!("loading a spec that is a FIFO waited for a writer for {limit:?}");
}

#[cfg(unix)]
#[test]
fn a_spec_that_is_a_fifo_is_refused_without_waiting_for_a_writer() {
    use nix::{sys::stat::Mode, unistd::mkfifo};

    let scratch = Scratch::new();
    let dir = directory_of_specs(&scratch);
    let fifo = dir.join("pipe.toml");
    mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("a FIFO");

    let error = load_within(&dir, "pipe", Duration::from_secs(10), &fifo)
        .expect_err("a FIFO is not a spec");

    assert!(matches!(error, SpecError::Read { .. }), "{error}");
    assert!(error.to_string().contains("not a regular file"), "{error}");
}

#[test]
fn a_spec_above_the_cap_is_refused_unread_and_one_at_the_cap_is_read() {
    let scratch = Scratch::new();
    let dir = directory_of_specs(&scratch);
    let cap = usize::try_from(MAX_SPEC_BYTES).expect("a cap that fits");
    // A spec padded with comments up to the cap, and one byte over it.
    let padded = |id: &str, bytes: usize| {
        let mut text = spec_text(id);
        text.push_str("# ");
        while text.len() < bytes - 1 {
            text.push('x');
        }
        text.push('\n');
        assert_eq!(text.len(), bytes);
        text
    };
    fs::write(dir.join("big.toml"), padded("big", cap)).expect("a spec at the cap");
    fs::write(dir.join("bigger.toml"), padded("bigger", cap + 1)).expect("a file over it");

    assert_eq!(
        FixtureSpec::load(&dir, "big").expect("at the cap").id,
        "big"
    );
    let text = refusal(&dir, "bigger");
    assert!(
        text.contains(&format!("larger than {MAX_SPEC_BYTES} bytes")),
        "{text}"
    );
    assert!(
        !text.contains("xxxx"),
        "the message does not repeat what was in the file: {text}"
    );
}
