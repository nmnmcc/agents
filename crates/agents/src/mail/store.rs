use super::transport;
use anyhow::{Context, Result, ensure};
use hopf_mailbox::{AppendGuard, Flag, IndexConfig, Mailbox, MboxMailbox, MessageReadCallback};
use mail_parser::MessageParser;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Messages to find in a folder. Deleted messages never match.
#[derive(Clone, Copy, Default)]
pub struct Query<'a> {
    pub unseen: bool,
    /// Keywords that must be set.
    pub required: &'a [&'a str],
    /// Keywords that must not be set.
    pub excluded: &'a [&'a str],
    /// A header that must contain the value, ignoring case.
    pub header: Option<(&'a str, &'a str)>,
    /// Only messages with a greater UID.
    pub after_uid: u32,
}

impl Query<'_> {
    pub fn imap(&self) -> Result<String> {
        let mut query = String::from("UNDELETED");
        if self.unseen {
            query.push_str(" UNSEEN");
        }
        for (prefix, keywords) in [("KEYWORD", self.required), ("UNKEYWORD", self.excluded)] {
            for keyword in keywords {
                ensure!(
                    !keyword.is_empty() && keyword.bytes().all(|b| b.is_ascii_alphanumeric()),
                    "invalid keyword {keyword:?}"
                );
                query.push_str(&format!(" {prefix} {keyword}"));
            }
        }
        if let Some((name, value)) = self.header {
            ensure!(
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "invalid header name {name:?}"
            );
            query.push_str(&format!(" HEADER {name} {}", quoted(value)?));
        }
        if self.after_uid > 0 {
            query.push_str(&format!(" UID {}:*", self.after_uid.saturating_add(1)));
        }
        Ok(query)
    }

    fn matches_flags(&self, flags: &[String]) -> bool {
        let has = |flag: &str| flags.iter().any(|value| value.eq_ignore_ascii_case(flag));
        !has("\\Deleted")
            && !(self.unseen && has("\\Seen"))
            && self.required.iter().all(|flag| has(flag))
            && !self.excluded.iter().any(|flag| has(flag))
    }

    fn matches_message(&self, raw: &[u8]) -> bool {
        let Some((name, value)) = self.header else {
            return true;
        };
        let value = value.to_lowercase();
        MessageParser::default()
            .parse_headers(raw)
            .is_some_and(|message| {
                message.headers_raw().any(|(header, text)| {
                    header.eq_ignore_ascii_case(name) && text.to_lowercase().contains(&value)
                })
            })
    }
}

pub fn quoted(text: &str) -> Result<String> {
    ensure!(!text.contains(['\r', '\n', '\0']), "invalid search text");
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub enum Folder {
    Imap {
        client: Box<transport::Imap>,
        folder: String,
        validity: u32,
    },
    Local {
        mailbox: Box<MboxMailbox>,
        sequences: BTreeMap<u32, u32>,
    },
}

impl Folder {
    pub fn imap(mut client: transport::Imap, folder: &str, keywords: &[&str]) -> Result<Self> {
        let validity = client.select(folder, keywords)?;
        Ok(Self::Imap {
            client: Box::new(client),
            folder: folder.into(),
            validity,
        })
    }

    pub fn local(root: &Path, directory: &Path, name: &str) -> Result<Self> {
        let path = prepare(root, directory, name)?;
        let mailbox = MboxMailbox::open(&path, name, false, IndexConfig::default())
            .with_context(|| format!("open local mbox {}", path.display()))?;
        let sequences = mailbox
            .messages()?
            .into_iter()
            .map(|message| {
                Ok((
                    mailbox.uid(message.message_number)?.try_into()?,
                    message.message_number,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self::Local {
            mailbox: Box::new(mailbox),
            sequences,
        })
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    pub fn validity(&self) -> Result<u32> {
        match self {
            Self::Imap { validity, .. } => Ok(*validity),
            Self::Local { mailbox, .. } => Ok(mailbox.uid_validity().try_into()?),
        }
    }

    /// UIDs of matching messages in ascending order.
    pub fn search(&mut self, query: &Query<'_>) -> Result<Vec<u32>> {
        match self {
            Self::Imap { client, .. } => {
                let mut uids = client.search(&query.imap()?)?;
                // "n:*" always includes the last message, even below n.
                uids.retain(|uid| *uid > query.after_uid);
                uids.sort_unstable();
                Ok(uids)
            }
            Self::Local { .. } => {
                let mut uids = Vec::new();
                let candidates: Vec<_> = self
                    .local_uids()
                    .filter(|uid| *uid > query.after_uid)
                    .collect();
                for uid in candidates {
                    if !query.matches_flags(&self.flags(uid)?) {
                        continue;
                    }
                    if query.header.is_some()
                        && !self
                            .message(uid)?
                            .is_some_and(|raw| query.matches_message(&raw))
                    {
                        continue;
                    }
                    uids.push(uid);
                }
                Ok(uids)
            }
        }
    }

    fn local_uids(&self) -> impl Iterator<Item = u32> + use<'_> {
        let Self::Local { sequences, .. } = self else {
            unreachable!("local folder")
        };
        sequences.keys().copied()
    }

    pub fn message(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Imap { client, .. } => client.message(uid),
            Self::Local { mailbox, sequences } => {
                let Some(&sequence) = sequences.get(&uid) else {
                    return Ok(None);
                };
                let mut contents = Contents::default();
                mailbox.read_message(sequence, &mut contents)?;
                Ok(Some(contents.0))
            }
        }
    }

    pub fn flags(&mut self, uid: u32) -> Result<Vec<String>> {
        match self {
            Self::Imap { client, .. } => client.flags(uid),
            Self::Local { mailbox, sequences } => local_flags(
                mailbox,
                *sequences.get(&uid).context("mbox message disappeared")?,
            ),
        }
    }

    pub fn set(&mut self, uid: u32, keyword: &str) -> Result<()> {
        self.keyword(uid, keyword, true)
    }

    pub fn keyword(&mut self, uid: u32, keyword: &str, add: bool) -> Result<()> {
        match self {
            Self::Imap { client, .. } => client.keyword(uid, keyword, add),
            Self::Local { mailbox, sequences } => {
                let sequence = *sequences.get(&uid).context("mbox message disappeared")?;
                mailbox.set_keywords(sequence, &BTreeSet::from([keyword.to_owned()]), add)?;
                mailbox.close(false)?;
                Ok(())
            }
        }
    }

    /// Permanently removes messages.
    pub fn expunge(&mut self, uids: &[u32]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        match self {
            Self::Imap { client, .. } => client.expunge(uids),
            Self::Local { mailbox, sequences } => {
                for uid in uids {
                    let sequence = *sequences.get(uid).context("mbox message disappeared")?;
                    mailbox.mark_deleted(sequence)?;
                }
                mailbox.close(true)?;
                let renumbered = mailbox
                    .messages()?
                    .into_iter()
                    .map(|message| {
                        Ok((
                            mailbox.uid(message.message_number)?.try_into()?,
                            message.message_number,
                        ))
                    })
                    .collect::<Result<_>>()?;
                *sequences = renumbered;
                Ok(())
            }
        }
    }

    pub fn set_read(&mut self, uid: u32, read: bool) -> Result<()> {
        match self {
            Self::Imap { client, .. } => client.set_read(uid, read),
            Self::Local { mailbox, sequences } => {
                let sequence = *sequences.get(&uid).context("mbox message disappeared")?;
                mailbox.set_flags(sequence, &BTreeSet::from([Flag::Seen]), read)?;
                mailbox.close(false)?;
                Ok(())
            }
        }
    }

    pub fn append(&mut self, keyword: Option<&'static str>, message: &[u8]) -> Result<()> {
        match self {
            Self::Imap { client, folder, .. } => client.append(folder, keyword, message),
            Self::Local { mailbox, sequences } => {
                let mut append = AppendGuard::start(mailbox.as_mut(), &BTreeSet::new(), None)?;
                append.append_content(message)?;
                let uid = append.commit()?.try_into()?;
                let sequence = u32::try_from(sequences.len() + 1)?;
                sequences.insert(uid, sequence);
                if let Some(keyword) = keyword {
                    self.set(uid, keyword)?;
                }
                Ok(())
            }
        }
    }
}

fn local_flags(mailbox: &MboxMailbox, sequence: u32) -> Result<Vec<String>> {
    Ok(mailbox
        .flags(sequence)?
        .into_iter()
        .map(|flag| flag.atom().to_owned())
        .chain(mailbox.keywords(sequence)?)
        .collect())
}

#[derive(Default)]
struct Contents(Vec<u8>);

impl MessageReadCallback for Contents {
    fn message_content(&mut self, chunk: &[u8]) -> bool {
        self.0.extend_from_slice(chunk);
        true
    }
}

pub(super) fn prepare(root: &Path, directory: &Path, name: &str) -> Result<PathBuf> {
    let mut path = root.to_owned();
    for part in directory.components() {
        let std::path::Component::Normal(part) = part else {
            anyhow::bail!("mail directory must be relative to the project");
        };
        path.push(part);
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                ensure!(
                    fs::symlink_metadata(&path)?.is_dir(),
                    "not a real mail directory: {}",
                    path.display()
                );
            }
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    ensure!(
        fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
        "mail directory must be private (mode 0700): {}",
        path.display()
    );
    path.push(name);
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(&path)?.is_file(),
                "not a regular mail state file: {}",
                path.display()
            );
        }
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_renders_imap_search() {
        let query = Query {
            unseen: true,
            required: &["AgentsReady"],
            excluded: &["AgentsSent", "AgentsFailed"],
            header: Some(("Message-ID", "<a\"b@example.test>")),
            after_uid: 7,
        };
        assert_eq!(
            query.imap().unwrap(),
            "UNDELETED UNSEEN KEYWORD AgentsReady UNKEYWORD AgentsSent \
             UNKEYWORD AgentsFailed HEADER Message-ID \"<a\\\"b@example.test>\" UID 8:*"
        );
        assert_eq!(Query::default().imap().unwrap(), "UNDELETED");
        for invalid in [
            Query {
                excluded: &["\\Seen"],
                ..Query::default()
            },
            Query {
                header: Some(("To:", "x")),
                ..Query::default()
            },
            Query {
                header: Some(("To", "a\r\nb")),
                ..Query::default()
            },
        ] {
            assert!(invalid.imap().is_err());
        }
    }

    #[test]
    fn local_search_evaluates_every_condition() {
        let dir = tempfile::tempdir().unwrap();
        let mut folder = Folder::local(dir.path(), Path::new("mail"), "inbox.mbox").unwrap();
        for (index, to) in ["a@example.test", "b@example.test", "A@Example.test"]
            .iter()
            .enumerate()
        {
            folder
                .append(
                    None,
                    format!("To: {to}\r\nMessage-ID: <{index}@example.test>\r\n\r\nbody\r\n")
                        .as_bytes(),
                )
                .unwrap();
        }
        let all = folder.search(&Query::default()).unwrap();
        assert_eq!(all.len(), 3);
        let (first, second, third) = (all[0], all[1], all[2]);
        folder.set_read(first, true).unwrap();
        folder.keyword(second, "AgentsDispatched", true).unwrap();
        let search = |folder: &mut Folder, query: Query<'_>| folder.search(&query).unwrap();
        let to_a = Some(("To", "a@EXAMPLE.test"));
        assert_eq!(
            search(
                &mut folder,
                Query {
                    header: to_a,
                    ..Query::default()
                }
            ),
            [first, third]
        );
        assert_eq!(
            search(
                &mut folder,
                Query {
                    unseen: true,
                    ..Query::default()
                }
            ),
            [second, third]
        );
        assert_eq!(
            search(
                &mut folder,
                Query {
                    excluded: &["agentsdispatched"],
                    ..Query::default()
                }
            ),
            [first, third]
        );
        assert_eq!(
            search(
                &mut folder,
                Query {
                    required: &["AgentsDispatched"],
                    ..Query::default()
                }
            ),
            [second]
        );
        assert_eq!(
            search(
                &mut folder,
                Query {
                    after_uid: first,
                    ..Query::default()
                }
            ),
            [second, third]
        );
        folder.keyword(second, "AgentsDispatched", false).unwrap();
        folder.expunge(&[first]).unwrap();
        assert_eq!(search(&mut folder, Query::default()), [second, third]);
        drop(folder);
        let mut reopened = Folder::local(dir.path(), Path::new("mail"), "inbox.mbox").unwrap();
        assert_eq!(
            search(
                &mut reopened,
                Query {
                    required: &["AgentsDispatched"],
                    ..Query::default()
                }
            ),
            [] as [u32; 0]
        );
        assert_eq!(search(&mut reopened, Query::default()), [second, third]);
    }
}
