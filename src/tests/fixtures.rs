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
        SNAPSHOT_PATHS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .expect("failed to lock snapshot path registry")
            .push((directory.path().to_path_buf(), name.to_owned()));
        Ok(Self { directory })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
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
