use super::transport;
use anyhow::{Context, Result, ensure};
use hopf_mailbox::{AppendGuard, Flag, IndexConfig, Mailbox, MboxMailbox, MessageReadCallback};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub enum Session {
    Imap {
        client: Box<transport::Session>,
        folder: String,
        validity: u32,
    },
    Local {
        mailbox: Box<MboxMailbox>,
        sequences: BTreeMap<u32, u32>,
    },
}

impl Session {
    pub fn imap(mut client: transport::Session, folder: &str, keywords: &[&str]) -> Result<Self> {
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

    // IMAP can narrow queries on the server. The local backend filters flags,
    // then callers inspect message headers and bodies as necessary.
    pub fn search(
        &mut self,
        query: &str,
        required: &[&str],
        excluded: &[&str],
    ) -> Result<Vec<u32>> {
        match self {
            Self::Imap { client, .. } => client.search(query),
            Self::Local { mailbox, sequences } => {
                let mut uids = Vec::new();
                for (&uid, &sequence) in sequences.iter() {
                    let flags = local_flags(mailbox, sequence)?;
                    let has =
                        |flag: &str| flags.iter().any(|value| value.eq_ignore_ascii_case(flag));
                    if !has("\\Deleted")
                        && required.iter().all(|flag| has(flag))
                        && !excluded.iter().any(|flag| has(flag))
                    {
                        uids.push(uid);
                    }
                }
                Ok(uids)
            }
        }
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
        match self {
            Self::Imap { client, .. } => client.set(uid, keyword),
            Self::Local { mailbox, sequences } => {
                let sequence = *sequences.get(&uid).context("mbox message disappeared")?;
                mailbox.set_keywords(sequence, &BTreeSet::from([keyword.to_owned()]), true)?;
                mailbox.close(false)?;
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
