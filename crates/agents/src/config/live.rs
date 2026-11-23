use super::{MailRoutes, Workspace};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::SystemTime,
};

/// A consistent view of the project configuration and the mail routes derived from it.
pub struct Snapshot {
    pub workspace: Workspace,
    pub routes: MailRoutes,
    digest: [u8; 32],
}

impl Snapshot {
    fn load(start: &Path) -> Result<Self> {
        let workspace = Workspace::load(Some(start))?;
        let routes = workspace.mail_routes()?;
        let mut digest = Sha256::new();
        digest.update(workspace.selected.sources.digest);
        routes.digests.iter().for_each(|part| digest.update(part));
        Ok(Self {
            workspace,
            routes,
            digest: digest.finalize().into(),
        })
    }

    fn watched(&self) -> impl Iterator<Item = &PathBuf> {
        self.workspace
            .selected
            .sources
            .paths
            .iter()
            .chain(&self.routes.watched)
    }
}

type Stamp = Vec<Option<(SystemTime, u64)>>;

/// Configuration shared between threads, reloaded when its source files change.
pub struct Live {
    start: PathBuf,
    current: RwLock<Arc<Snapshot>>,
    stamp: Mutex<Stamp>,
}

impl Live {
    pub fn load(directory: Option<&Path>) -> Result<Self> {
        let start = match directory {
            Some(directory) => directory.to_owned(),
            None => std::env::current_dir()?,
        };
        let snapshot = Snapshot::load(&start)?;
        let stamp = stamp(snapshot.watched());
        Ok(Self {
            start,
            current: RwLock::new(Arc::new(snapshot)),
            stamp: Mutex::new(stamp),
        })
    }

    pub fn get(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Reloads the configuration if a watched file changed. Returns the new snapshot when the
    /// effective configuration differs. A configuration that fails to load leaves the current
    /// snapshot in place and is retried once its files change again.
    pub fn refresh(&self) -> Result<Option<Arc<Snapshot>>> {
        let mut stamp = self.stamp.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.get();
        let observed = self::stamp(current.watched());
        if observed == *stamp {
            return Ok(None);
        }
        *stamp = observed;
        let next = Snapshot::load(&self.start)?;
        // Reloading may discover new layers or projects; watch them from now on.
        *stamp = self::stamp(next.watched());
        if next.digest == current.digest {
            return Ok(None);
        }
        let next = Arc::new(next);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&next);
        Ok(Some(next))
    }
}

fn stamp<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Stamp {
    paths
        .map(|path| match fs::metadata(path) {
            Ok(metadata) => Some((
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                metadata.len(),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => Some((SystemTime::UNIX_EPOCH, u64::MAX)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn reloads_only_effective_changes_and_keeps_the_last_good_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/project");
        let root = dir.path().join("project");
        assert!(
            Command::new("cp")
                .arg("-R")
                .args([&sample, &root])
                .status()
                .unwrap()
                .success()
        );
        let main = root.join(".agents.json");
        let original = fs::read_to_string(&main).unwrap();
        let live = Arc::new(Live::load(Some(&root)).unwrap());
        assert!(live.refresh().unwrap().is_none());
        let shared = Arc::clone(&live);
        let description = std::thread::spawn(move || {
            shared
                .get()
                .workspace
                .project(None)
                .unwrap()
                .agent
                .id
                .to_string()
        });
        assert_eq!(description.join().unwrap(), "reviewer");

        // Formatting is not an effective change.
        fs::write(&main, format!("{original}\n\n")).unwrap();
        assert!(live.refresh().unwrap().is_none());

        let changed = original.replace("Review a change", "Inspect a change");
        fs::write(&main, &changed).unwrap();
        let next = live.refresh().unwrap().expect("reloaded");
        assert!(Arc::ptr_eq(&next, &live.get()));

        // A broken edit keeps the current configuration until it is fixed.
        fs::write(&main, "{").unwrap();
        assert!(live.refresh().is_err());
        assert!(Arc::ptr_eq(&next, &live.get()));
        assert!(live.refresh().unwrap().is_none());

        // New layers are discovered and watched.
        fs::write(&main, &changed).unwrap();
        fs::write(
            root.join(".agents.d/10-timeout.json"),
            r#"{"agents": {"reviewer": {"timeout": 5}}}"#,
        )
        .unwrap();
        let layered = live.refresh().unwrap().expect("reloaded layer");
        assert_eq!(
            layered.workspace.project(None).unwrap().timeout,
            Some(std::time::Duration::from_secs(5))
        );
    }
}
