use super::{inbox::MailEvent, store};
use crate::{harness, types::Id};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex},
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
struct Native {
    harness: Id,
    id: String,
}

#[derive(Debug)]
pub struct SessionBusy;

impl std::fmt::Display for SessionBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("agent already has a running mail session")
    }
}

impl std::error::Error for SessionBusy {}

pub struct Sessions {
    _lock: fs::File,
    path: PathBuf,
    records: BTreeMap<String, Native>,
    current: Native,
    triggered_by_mail: bool,
}

#[derive(Default)]
pub struct SessionId {
    observed: Mutex<Option<Option<String>>>,
    changed: Condvar,
}

impl SessionId {
    pub fn finish(&self, id: Option<String>) {
        let mut state = self.observed.lock().unwrap();
        if state.is_none() {
            *state = Some(id);
            self.changed.notify_all();
        }
    }

    pub fn get(&self) -> Result<String> {
        let (state, _) = self
            .changed
            .wait_timeout_while(
                self.observed.lock().unwrap(),
                Duration::from_secs(5),
                |state| state.is_none(),
            )
            .unwrap();
        state
            .as_ref()
            .cloned()
            .flatten()
            .context("harness has not reported a native session ID")
    }
}

impl Sessions {
    pub fn open(
        root: &Path,
        directory: &Path,
        agent: &Id,
        harness: &Id,
        event: Option<&MailEvent>,
    ) -> Result<(Self, harness::Session)> {
        let lock = store::prepare(root, directory, &format!("sessions-{agent}.lock"))?;
        let lock = fs::OpenOptions::new().read(true).write(true).open(lock)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Err(SessionBusy.into()),
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).context("lock agent mail sessions");
            }
        }
        let path = store::prepare(root, directory, &format!("sessions-{agent}.json"))?;
        let bytes = fs::read(&path)?;
        let mut records: BTreeMap<String, Native> = if bytes.is_empty() {
            BTreeMap::new()
        } else {
            serde_json::from_slice(&bytes).context("decode mail session bindings")?
        };
        let keys = event.map(MailEvent::keys).unwrap_or_default();
        let mut matches = keys.iter().filter_map(|key| records.get(key));
        let existing = matches.next().cloned();
        let resume = existing.is_some();
        let current = if let Some(existing) = existing {
            ensure!(
                matches.all(|other| other.id == existing.id),
                "mail references more than one local agent session"
            );
            ensure!(
                &existing.harness == harness,
                "original mail session uses a different harness"
            );
            existing
        } else {
            Native {
                harness: harness.clone(),
                id: Uuid::new_v4().to_string(),
            }
        };
        for key in keys {
            records.insert(key, current.clone());
        }
        let session = harness::Session {
            id: current.id.clone(),
            resume,
            directory: path
                .parent()
                .context("mail state directory missing")?
                .to_owned(),
        };
        Ok((
            Self {
                _lock: lock,
                path,
                records,
                current,
                triggered_by_mail: event.is_some(),
            },
            session,
        ))
    }

    pub fn bind_outgoing(&mut self, keys: impl IntoIterator<Item = String>) -> Result<()> {
        for key in keys {
            self.records.insert(key, self.current.clone());
        }
        self.save()
    }

    pub fn set_native_id(&mut self, id: String) {
        for session in self
            .records
            .values_mut()
            .filter(|session| session.id == self.current.id)
        {
            session.id.clone_from(&id);
        }
        self.current.id = id;
    }

    pub fn reply_keys(&self, event: &MailEvent) -> Result<Vec<String>> {
        let keys = event.keys();
        let mut same_session = false;
        for native in keys.iter().filter_map(|key| self.records.get(key)) {
            ensure!(
                native.id == self.current.id,
                "reply belongs to another agent session"
            );
            same_session = true;
        }
        ensure!(
            !self.triggered_by_mail || same_session,
            "reply does not belong to the active mail thread"
        );
        Ok(keys)
    }

    pub fn save(&self) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(
            self.path.parent().context("mail state directory missing")?,
        )?;
        serde_json::to_writer(file.as_file_mut(), &self.records)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .context("save mail session bindings")?;
        Ok(())
    }
}

pub fn message_key(sender: &str, id: &str) -> String {
    format!("{sender}\nmessage:{id}")
}
