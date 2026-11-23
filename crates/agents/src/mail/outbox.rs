use super::{
    inbox::unique_header,
    retry,
    store::Folder,
    transport::{self, Submission},
};
use crate::{
    config::{MailRoutes, Workspace},
    types::MailAddress,
};
use anyhow::{Context, Result, ensure};
use mail_parser::{Address, MessageParser};
use serde::Serialize;

const READY: &str = "AgentsReady";
const SENT: &str = "AgentsSent";
const SEND_FAILED: &str = "AgentsSendFailed";
const EXPIRED: &str = "AgentsExpired";
const SUPERSEDED: &str = "AgentsSuperseded";
pub(super) const FLAGS: &[&str] = &[READY, SENT, SEND_FAILED, EXPIRED, SUPERSEDED];
const TERMINAL: &[&str] = &[SENT, SEND_FAILED, EXPIRED, SUPERSEDED];

#[derive(Serialize)]
pub struct Pending {
    outbox_uid_validity: u32,
    queued: Vec<u32>,
    rejected: Vec<u32>,
    expired: Vec<u32>,
}

pub fn pending(workspace: &Workspace) -> Result<Pending> {
    let mut store = workspace
        .mail()
        .open(workspace.root(), workspace.mail_directory(), true)?;
    Ok(Pending {
        outbox_uid_validity: store.validity()?,
        queued: store.search(&ready_query(), &[], TERMINAL)?,
        rejected: store.search(
            &format!("UNDELETED KEYWORD {SEND_FAILED} UNKEYWORD {SENT}"),
            &[SEND_FAILED],
            &[SENT],
        )?,
        expired: store.search(&format!("UNDELETED KEYWORD {EXPIRED}"), &[EXPIRED], &[])?,
    })
}

pub fn flush(workspace: &Workspace) -> Result<()> {
    let mail = workspace.mail();
    let routes = workspace.mail_routes()?;
    let mut smtp = None;
    let mut store = mail.open(workspace.root(), workspace.mail_directory(), true)?;
    let mut failed = false;
    for uid in store.search(&ready_query(), &[], TERMINAL)? {
        let Some(raw) = store.message(uid)? else {
            continue;
        };
        let Ok((mut entry, id, from, to)) = envelope(&routes, &raw) else {
            store.set(uid, SEND_FAILED)?;
            eprintln!("agents: invalid outbox UID {uid}; see mail pending");
            failed = true;
            continue;
        };
        if superseded(&routes, &mut store, uid, &id, &to)? {
            store.set(uid, SUPERSEDED)?;
            continue;
        }
        let now = retry::now()?;
        if mail.retry.expired(entry.created, now) {
            store.set(uid, EXPIRED)?;
            failed = true;
            continue;
        }
        if entry.due > now {
            continue;
        }
        let local = if store.is_local() {
            routes
                .route(&to)
                .ok()
                .map(Workspace::at_mailbox)
                .filter(|target| target.mail().imap.is_none())
        } else {
            None
        };
        let outcome = if let Some(target) = local {
            let mut inbox = target
                .mail()
                .open(target.root(), target.mail_directory(), false)?;
            inbox.append(None, &stamp(&to, &from, entry.message)?)?;
            Submission::Accepted
        } else if let Some(config) = &mail.smtp {
            if smtp.is_none() {
                smtp = Some(transport::submission(config, workspace.root())?);
            }
            transport::send(
                smtp.as_ref().expect("SMTP initialized"),
                &from.to_string(),
                &to.to_string(),
                entry.message,
            )?
        } else {
            eprintln!("agents: SMTP is not configured for outbox UID {uid}; mail remains queued");
            failed = true;
            continue;
        };
        match outcome {
            Submission::Accepted => {
                store.set(uid, SENT)?;
                continue;
            }
            Submission::Temporary => {
                if let Some(due) = mail.retry.next_due(entry.created, retry::now()?) {
                    entry.due = due;
                    append(&mut store, &entry.encode())?;
                    store.set(uid, SUPERSEDED)?;
                } else {
                    store.set(uid, EXPIRED)?;
                }
            }
            Submission::Rejected => store.set(uid, SEND_FAILED)?,
        }
        failed = true;
        eprintln!("agents: delivery failed for outbox UID {uid}; see mail pending");
    }
    ensure!(!failed, "some outgoing mail was not delivered");
    Ok(())
}

pub(super) fn enqueue(store: &mut Folder, raw: &[u8], to: &MailAddress) -> Result<()> {
    append(
        store,
        &retry::QueueEntry::new(raw, retry::now()?, &to.to_string()).encode(),
    )
}

fn append(store: &mut Folder, entry: &[u8]) -> Result<()> {
    // Every record in the dedicated local outbox is initially queued.
    // Avoid a second write between the durable append and the ready state.
    store.append((!store.is_local()).then_some(READY), entry)
}

pub fn deliver(
    workspace: &Workspace,
    to: &MailAddress,
    from: &MailAddress,
    raw: &[u8],
) -> Result<()> {
    let routes = workspace.mail_routes()?;
    let target = Workspace::at_mailbox(routes.route(to)?);
    let stamped = stamp(to, from, raw)?;
    target
        .mail()
        .open(target.root(), target.mail_directory(), false)?
        .append(None, &stamped)
}

fn stamp(to: &MailAddress, from: &MailAddress, raw: &[u8]) -> Result<Vec<u8>> {
    let message = MessageParser::default()
        .parse(raw)
        .context("invalid RFC 5322 mail")?;
    for name in ["X-Agents-Envelope-To", "X-Agents-Authenticated-From"] {
        ensure!(
            unique_header(&message, name)?.is_none(),
            "message already contains {name}"
        );
    }
    let mut stamped =
        format!("X-Agents-Envelope-To: {to}\r\nX-Agents-Authenticated-From: {from}\r\n")
            .into_bytes();
    stamped.extend_from_slice(raw);
    Ok(stamped)
}

fn ready_query() -> String {
    format!(
        "UNDELETED KEYWORD {READY} UNKEYWORD {SENT} UNKEYWORD {SEND_FAILED} UNKEYWORD {EXPIRED} UNKEYWORD {SUPERSEDED}"
    )
}

fn envelope<'a>(
    routes: &MailRoutes,
    raw: &'a [u8],
) -> Result<(retry::QueueEntry<'a>, String, MailAddress, MailAddress)> {
    let entry = retry::QueueEntry::read(raw)?;
    let message = MessageParser::default()
        .parse(entry.message)
        .context("invalid queued message")?;
    ensure!(
        matches!(unique_header(&message, "X-Agents-Kind")?, Some("task")),
        "not a worker-generated message"
    );
    let address = |items: Option<&Address<'_>>| -> Result<MailAddress> {
        let items = items
            .and_then(Address::as_list)
            .context("queued mail requires one mailbox")?;
        ensure!(items.len() == 1, "queued mail requires one mailbox");
        items[0]
            .address()
            .context("invalid queued address")?
            .parse()
            .map_err(anyhow::Error::msg)
    };
    let from = address(message.from())?;
    routes.route(&from)?;
    let to = entry.recipient.parse().map_err(anyhow::Error::msg)?;
    let id = unique_header(&message, "Message-ID")?
        .and_then(message_id)
        .context("invalid queued Message-ID")?;
    Ok((entry, id, from, to))
}

fn superseded(
    routes: &MailRoutes,
    store: &mut Folder,
    uid: u32,
    id: &str,
    to: &MailAddress,
) -> Result<bool> {
    for other in store
        .search(
            &format!("KEYWORD {READY} HEADER Message-ID \"{id}\""),
            &[],
            &[],
        )?
        .into_iter()
        .filter(|other| *other > uid)
    {
        let Some(raw) = store.message(other)? else {
            continue;
        };
        let Ok((_, other_id, _, other_to)) = envelope(routes, &raw) else {
            continue;
        };
        if other_id == id && other_to == *to {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn message_id(value: &str) -> Option<String> {
    let value = value.trim_matches(['<', '>']);
    let (left, right) = value.split_once('@')?;
    if left.is_empty()
        || right.is_empty()
        || right.contains('@')
        || !value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b',' | b';' | b'"' | b'\\'))
    {
        return None;
    }
    Some(value.to_owned())
}
