use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tempfile::{Builder, TempDir};

static SNAPSHOT_PATHS: OnceLock<Mutex<Vec<(PathBuf, String)>>> = OnceLock::new();

pub struct TestDirectory {
    directory: TempDir,
}

impl TestDirectory {
    pub fn new(name: &str) -> anyhow::Result<Self> {
        let directory = Builder::new()
            .prefix(&format!("excise-{name}-"))
            .tempdir()?;
        register_snapshot_root(directory.path(), name);
        Ok(Self { directory })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Makes displayed paths under `root` read `/tmp/excise_tests/<name>/...`, so frames do not
/// depend on where the directory lives.
pub fn register_snapshot_root(root: &Path, name: &str) {
    SNAPSHOT_PATHS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("failed to lock snapshot path registry")
        .push((root.to_path_buf(), name.to_owned()));
}

pub fn snapshot_path(path: &Path) -> String {
    let Some(paths) = SNAPSHOT_PATHS.get() else {
        return path.to_string_lossy().replace('\\', "/");
    };
    let paths = paths.lock().expect("failed to lock snapshot path registry");
    for (actual_root, fixture_name) in paths.iter() {
        if let Ok(relative) = path.strip_prefix(actual_root) {
            let mut rendered = format!("/tmp/excise_tests/{fixture_name}");
            for component in relative.components() {
                rendered.push('/');
                rendered.push_str(&component.as_os_str().to_string_lossy());
            }
            return rendered;
        }
    }
    path.to_string_lossy().replace('\\', "/")
}
