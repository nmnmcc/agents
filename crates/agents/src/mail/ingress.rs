use super::{
    MailAccess, dkim,
    inbox::{INGESTED, LOCAL_FROM, LOCAL_TO, UNSIGNED, addresses, unique_header},
    retry,
    signatures::{LIMIT, PROTECTED, Verified},
    store::{self, Folder, Query},
    transport,
};
use crate::{
    config::{MailRoutes, Workspace},
    types::MailAddress,
};
use anyhow::{Context, Result, bail, ensure};
use mail_parser::{MessageParser, MimeHeaders, PartType};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs::{self, OpenOptions},
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Keyword set on source messages that were not imported.
const REJECTED: &str = "AgentsRejected";
const RECEIPTS: &str = "receipts";
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Receives mail from the configured IMAP inbox over one long-lived connection.
/// Unseen messages are imported or rejected, then marked seen.
#[derive(Default)]
pub struct ImapIngress {
    source: Option<Source>,
    failures: u32,
    retry_at: Option<Instant>,
}

struct Source {
    imap: transport::Imap,
    idle: bool,
}

impl ImapIngress {
    /// Drops the connection, for example because the configuration changed.
    pub fn reset(&mut self) {
        self.source = None;
        self.failures = 0;
        self.retry_at = None;
    }

    /// Imports every unseen message. Connection failures are retried with exponential backoff.
    pub fn receive(&mut self, workspace: &Workspace, routes: &MailRoutes) -> Result<()> {
        let Some(config) = &workspace.mail().imap else {
            self.source = None;
            return Ok(());
        };
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return Ok(());
        }
        let result = self.drain(workspace, routes, config);
        if result.is_err() {
            self.source = None;
            self.failures = self.failures.saturating_add(1);
            let delay = Duration::from_secs(1 << self.failures.min(9)).min(MAX_BACKOFF);
            self.retry_at = Some(Instant::now() + delay);
        } else {
            self.failures = 0;
            self.retry_at = None;
        }
        result
    }

    fn drain(
        &mut self,
        workspace: &Workspace,
        routes: &MailRoutes,
        config: &transport::Incoming,
    ) -> Result<()> {
        if self.source.is_none() {
            let mut imap = transport::connect(config, workspace.root())?;
            imap.select(&config.mailbox, &[])?;
            let idle = imap.supports_idle()?;
            self.source = Some(Source { imap, idle });
        }
        let imap = &mut self.source.as_mut().expect("connected").imap;
        let unseen = Query {
            unseen: true,
            ..Query::default()
        };
        for uid in imap.search(&unseen.imap()?)? {
            let Some(raw) = imap.message_prefix(uid, LIMIT + 1)? else {
                continue;
            };
            match Letter::read(workspace, routes, None, Mode::Any, &raw) {
                // A local failure leaves the message unseen to retry it.
                Ok(letter) => letter.import(workspace)?,
                Err(error) => {
                    eprintln!("agents: rejected IMAP UID {uid}: {error:#}");
                    if let Err(error) = imap.keyword(uid, REJECTED, true) {
                        eprintln!("agents: cannot flag rejected IMAP UID {uid}: {error:#}");
                    }
                }
            }
            imap.set_read(uid, true)?;
        }
        Ok(())
    }

    /// Waits up to `timeout` for new mail, using IMAP IDLE when the server supports it.
    pub fn wait(&mut self, timeout: Duration) {
        let deadline = self.retry_at.map_or(timeout, |at| {
            at.saturating_duration_since(Instant::now()).min(timeout)
        });
        match &mut self.source {
            Some(source) if source.idle => {
                if let Err(error) = source.imap.idle(deadline) {
                    eprintln!("agents: receive: {error:#}");
                    self.source = None;
                }
            }
            _ => thread::sleep(deadline),
        }
    }
}

/// Imports a message delivered by a local MDA or by hand.
pub fn deliver(
    workspace: &Workspace,
    recipient: Option<&MailAddress>,
    unsigned: bool,
    raw: &[u8],
) -> Result<()> {
    let routes = workspace.mail_routes()?;
    let mode = if unsigned {
        Mode::Unsigned
    } else {
        Mode::Authenticated
    };
    Letter::read(workspace, &routes, recipient, mode, raw)?.import(workspace)
}

/// Which kinds of mail `Letter::read` accepts.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    /// Signed or DKIM-authenticated mail, and unsigned mail when that is allowed.
    Any,
    /// Only signed or DKIM-authenticated mail.
    Authenticated,
    /// Only unsigned mail, when that is allowed.
    Unsigned,
}

/// How the sender of an accepted message was established.
enum Origin {
    Signed(Verified),
    Dkim(MailAddress, dkim::Authenticated),
    Unsigned,
}

/// An accepted incoming message and the inbox records it becomes.
struct Letter {
    /// Identifies the message independent of its recipient.
    receipt: [u8; 32],
    /// When the receipt may be forgotten; later replays fail signature age checks.
    expires: u64,
    unsigned: bool,
    sender: MailAddress,
    recipients: Vec<MailAddress>,
    /// Headers and body of the imported message, without local routing headers.
    message: Vec<u8>,
}

impl Letter {
    fn read(
        workspace: &Workspace,
        routes: &MailRoutes,
        recipient: Option<&MailAddress>,
        mode: Mode,
        raw: &[u8],
    ) -> Result<Self> {
        ensure!(raw.len() <= LIMIT, "mail exceeds size limit");
        let mail = workspace.mail();
        let outer = MessageParser::default()
            .parse(raw)
            .context("invalid RFC 5322 mail")?;
        let from = || -> Result<MailAddress> {
            let [sender] = addresses(outer.from())
                .context("invalid From")?
                .try_into()
                .ok()
                .context("mail without a signature requires exactly one From mailbox")?;
            Ok(sender)
        };
        let origin = if mode == Mode::Unsigned {
            Origin::Unsigned
        } else if !looks_unsigned(&outer) {
            Origin::Signed(mail.signatures.verify(workspace.root(), raw)?)
        } else if mail.dkim.enabled() {
            let sender = from()?;
            match mail.dkim.authenticate(&outer, sender.domain()) {
                Ok(authenticated) => Origin::Dkim(sender, authenticated),
                Err(_) if mode == Mode::Any && mail.dangerously_allow_unsigned => Origin::Unsigned,
                Err(error) => return Err(error.context("mail is not authenticated by DKIM")),
            }
        } else if mode == Mode::Any {
            Origin::Unsigned
        } else {
            bail!("mail is not signed with OpenPGP/MIME or S/MIME and DKIM is not configured")
        };
        let unsigned = matches!(origin, Origin::Unsigned);
        // Headers a DKIM signature does not cover are not authenticated and are dropped.
        let covered: Option<BTreeSet<String>> = match &origin {
            Origin::Dkim(_, authenticated) => Some(authenticated.covered.clone()),
            _ => None,
        };
        let trusted = |name: &str| {
            covered
                .as_ref()
                .is_none_or(|covered| covered.contains(&name.to_ascii_lowercase()))
        };
        let (receipt, expires, sender, entity) = match origin {
            Origin::Unsigned => {
                ensure!(
                    looks_unsigned(&outer),
                    "signed or encrypted mail is never accepted as unsigned"
                );
                ensure!(
                    mail.dangerously_allow_unsigned,
                    "mail is unsigned and unsigned ingress is disabled \
                     (mail.transport.dangerously_allow_unsigned)"
                );
                let sender = from()?;
                (Sha256::digest(raw).into(), u64::MAX, sender, raw.to_vec())
            }
            Origin::Signed(verified) => (
                verified.receipt,
                verified.signed_at.saturating_add(mail.signatures.max_age()),
                verified.signer,
                verified.entity,
            ),
            Origin::Dkim(sender, authenticated) => {
                let signed_at = match authenticated.signed_at {
                    Some(time) => time,
                    None if trusted("Date") => outer
                        .date()
                        .and_then(|date| u64::try_from(date.to_timestamp()).ok())
                        .context("DKIM-authenticated mail has no valid Date")?,
                    None => bail!("DKIM signature has no timestamp and does not cover Date"),
                };
                mail.signatures.check_age(signed_at)?;
                let mut digest = Sha256::new();
                digest.update(b"dkim\0");
                digest.update(&authenticated.signature);
                (
                    digest.finalize().into(),
                    signed_at.saturating_add(mail.signatures.max_age()),
                    sender,
                    raw.to_vec(),
                )
            }
        };
        let inner = MessageParser::default()
            .parse(&entity)
            .context("invalid signed MIME entity")?;
        // A signed Message-ID names the message even when the sender re-signs it to retry.
        let receipt = match unique_header(&inner, "Message-ID")? {
            Some(id) if !unsigned && trusted("Message-ID") => {
                let mut digest = Sha256::new();
                digest.update(sender.routing_key());
                digest.update([0]);
                digest.update(id.trim());
                digest.finalize().into()
            }
            _ => receipt,
        };
        let header = |name: &str| -> Option<String> {
            if !trusted(name) {
                return None;
            }
            let find = |message: &mail_parser::Message<'_>| {
                message
                    .headers_raw()
                    .find(|(header, _)| header.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim().to_owned())
            };
            // Signed entities may carry protected headers that the signature covers.
            find(&inner).or_else(|| match name {
                // Unauthenticated outer headers must not redirect replies to signed mail.
                "Reply-To" if !unsigned => None,
                _ => find(&outer),
            })
        };
        let protected_recipients = !unsigned
            && inner.headers_raw().any(|(name, _)| {
                (name.eq_ignore_ascii_case("To") || name.eq_ignore_ascii_case("Cc"))
                    && trusted(name)
            });
        let mut message = format!("From: {sender}\r\n");
        for name in PROTECTED.iter().filter(|name| **name != "From") {
            let value = match (*name, header(name)) {
                (_, Some(value)) => value,
                ("Message-ID", None) => format!(
                    "<{}@{}.invalid>",
                    hex(&receipt),
                    if unsigned { "unsigned" } else { "signed" }
                ),
                _ => continue,
            };
            ensure!(!value.contains('\0'), "invalid {name} header");
            writeln!(message, "{name}: {}\r", value.replace(['\r', '\n'], " "))?;
        }
        let mut record = message.into_bytes();
        let part = inner.parts.first().context("empty signed MIME entity")?;
        for (name, value) in inner.headers_raw() {
            let lower = name.to_ascii_lowercase();
            if PROTECTED
                .iter()
                .any(|protected| protected.eq_ignore_ascii_case(name))
                || lower.starts_with("x-agents-")
            {
                continue;
            }
            record.extend_from_slice(format!("{name}:{value}").as_bytes());
            if !record.ends_with(b"\n") {
                record.extend_from_slice(b"\r\n");
            }
        }
        record.extend_from_slice(b"\r\n");
        record.extend_from_slice(
            entity
                .get(part.raw_body_offset() as usize..)
                .context("invalid signed MIME entity")?,
        );
        let headers = MessageParser::default()
            .parse_headers(&record)
            .context("invalid imported headers")?;
        let mut listed = addresses(headers.to()).context("invalid To")?;
        listed.extend(addresses(headers.cc()).context("invalid Cc")?);
        let candidates = match recipient {
            Some(recipient) => {
                ensure!(
                    !protected_recipients || listed.iter().any(|a| a.matches(recipient)),
                    "{recipient} is not a recipient of the signed mail"
                );
                vec![recipient.clone()]
            }
            None => listed,
        };
        let mut recipients: Vec<MailAddress> = Vec::new();
        for address in candidates {
            let Ok(mailbox) = routes.route(&address) else {
                continue;
            };
            if mailbox.project.root != workspace.root()
                || recipients.iter().any(|known| known.matches(&address))
            {
                continue;
            }
            // Pinned signers are trusted by configuration; a DKIM domain vouches for anyone
            // with a mailbox there, so those senders must be listed in mail.trust.
            if MailAccess::new(mail, routes, mailbox)?
                .incoming
                .allows(&sender, covered.is_none())
            {
                recipients.push(mailbox.address().clone());
            } else {
                eprintln!("agents: {sender} may not send mail to {address}");
            }
        }
        ensure!(
            !recipients.is_empty(),
            "no permitted recipient in this project"
        );
        Ok(Self {
            receipt,
            expires,
            unsigned,
            sender,
            recipients,
            message: crlf(&record),
        })
    }

    /// Appends one inbox record per recipient unless an earlier delivery already did.
    fn import(self, workspace: &Workspace) -> Result<()> {
        let root = workspace.root();
        let directory = workspace.mail_directory();
        let lock = store::prepare(root, directory, "import.lock")?;
        let lock = OpenOptions::new().write(true).open(lock)?;
        lock.lock()?;
        let mut inbox = workspace.mail().open(root, directory, false)?;
        let path = store::prepare(root, directory, RECEIPTS)?;
        let mut receipts = Receipts::load(&path, &mut inbox)?;
        for recipient in &self.recipients {
            let key = {
                let mut digest = Sha256::new();
                digest.update(self.receipt);
                digest.update(recipient.routing_key());
                hex(&digest.finalize())
            };
            if receipts.entries.contains_key(&key) {
                continue;
            }
            // The receipt is part of the record, so the index can always be rebuilt from it.
            let mut record = format!(
                "{INGESTED}: {key}; expires={}\r\n{LOCAL_TO}: {recipient}\r\n{LOCAL_FROM}: {}\r\n",
                self.expires, self.sender
            );
            if self.unsigned {
                record.push_str(&format!("{UNSIGNED}: yes\r\n"));
            }
            let mut record = record.into_bytes();
            record.extend_from_slice(&self.message);
            inbox.append(None, &record)?;
            receipts.entries.insert(key, self.expires);
        }
        receipts.save(&path)
    }
}

/// Receipts of imported mail, built incrementally from the inbox.
struct Receipts {
    validity: u32,
    last: u32,
    entries: BTreeMap<String, u64>,
}

impl Receipts {
    fn load(path: &Path, inbox: &mut Folder) -> Result<Self> {
        let validity = inbox.validity()?;
        let text = fs::read_to_string(path)?;
        let mut lines = text.lines();
        let mut receipts = match lines.next().and_then(|line| line.split_once(' ')) {
            Some((stored, last)) if stored.parse() == Ok(validity) => Self {
                validity,
                last: last.parse().context("corrupt receipt index")?,
                entries: lines
                    .map(|line| {
                        let (key, expires) = line.split_once(' ').context("corrupt receipt")?;
                        Ok((key.to_owned(), expires.parse()?))
                    })
                    .collect::<Result<_>>()?,
            },
            _ => Self {
                validity,
                last: 0,
                entries: BTreeMap::new(),
            },
        };
        for uid in inbox.search(&Query {
            after_uid: receipts.last,
            ..Query::default()
        })? {
            if let Some(raw) = inbox.message(uid)?
                && let Some(message) = MessageParser::default().parse_headers(&raw)
                && let Ok(Some(value)) = unique_header(&message, INGESTED)
            {
                let (key, expires) = value.split_once("; expires=").unwrap_or((value, ""));
                receipts
                    .entries
                    .insert(key.to_owned(), expires.parse().unwrap_or(u64::MAX));
            }
            receipts.last = uid;
        }
        Ok(receipts)
    }

    fn save(mut self, path: &Path) -> Result<()> {
        let now = retry::now()?;
        self.entries.retain(|_, expires| *expires > now);
        let mut text = format!("{} {}\n", self.validity, self.last);
        for (key, expires) in &self.entries {
            writeln!(text, "{key} {expires}")?;
        }
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, text)?;
        fs::rename(temporary, path)?;
        Ok(())
    }
}

/// Whether the message has no signature or encryption that verification should consider.
/// Invalid signed or encrypted mail is never reinterpreted as unsigned.
fn looks_unsigned(message: &mail_parser::Message<'_>) -> bool {
    !message.parts.iter().any(|part| {
        part.content_type().is_some_and(|ty| {
            let subtype = ty.subtype().unwrap_or_default().to_ascii_lowercase();
            subtype.contains("pkcs7")
                || matches!(
                    subtype.as_str(),
                    "signed" | "encrypted" | "pgp-signature" | "pgp-encrypted"
                )
        }) || matches!(&part.body, PartType::Text(text) if text.contains("-----BEGIN PGP "))
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r') {
            output.push(b'\r');
        }
        output.push(byte);
    }
    output
}
