use super::{
    inbox::{LOCAL_FROM, LOCAL_TO, unique_header},
    retry,
    store::{Folder, Query},
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
        queued: store.search(&READY_QUERY)?,
        rejected: store.search(&Query {
            required: &[SEND_FAILED],
            excluded: &[SENT],
            ..Query::default()
        })?,
        expired: store.search(&Query {
            required: &[EXPIRED],
            ..Query::default()
        })?,
    })
}

/// Delivers due outbox records. A message that cannot be delivered is retried or marked
/// failed without holding up the others. Returns whether every due message was delivered.
pub fn flush(workspace: &Workspace) -> Result<bool> {
    let mail = workspace.mail();
    let routes = workspace.mail_routes()?;
    let mut smtp = None;
    let mut store = mail.open(workspace.root(), workspace.mail_directory(), true)?;
    let mut delivered = true;
    for uid in store.search(&READY_QUERY)? {
        let Some(raw) = store.message(uid)? else {
            continue;
        };
        let (mut entry, id, from, to) = match envelope(&routes, &raw) {
            Ok(envelope) => envelope,
            Err(error) => {
                eprintln!("agents: invalid outbox UID {uid}: {error:#}");
                store.set(uid, SEND_FAILED)?;
                delivered = false;
                continue;
            }
        };
        if superseded(&routes, &mut store, uid, &id, &to)? {
            store.set(uid, SUPERSEDED)?;
            continue;
        }
        let now = retry::now()?;
        if mail.retry.expired(entry.created, now) {
            eprintln!("agents: outbox UID {uid} to {to} expired");
            store.set(uid, EXPIRED)?;
            delivered = false;
            continue;
        }
        if entry.due > now {
            continue;
        }
        let outcome = match routes.route(&to).ok().map(Workspace::at_mailbox) {
            Some(target) => deliver_local(&target, &from, &to, entry.message),
            None => match &mail.smtp {
                Some(config) => {
                    let smtp_from = config.from.as_ref().unwrap_or(&from);
                    if smtp.is_none() {
                        smtp = Some(transport::submission(config, workspace.root())?);
                    }
                    mail.signatures
                        .sign(workspace.root(), &from, smtp_from, entry.message)
                        .context("sign mail")
                        .and_then(|signed| {
                            transport::send(
                                smtp.as_ref().expect("SMTP initialized"),
                                &smtp_from.to_string(),
                                &to.to_string(),
                                &signed,
                            )
                        })
                }
                None => Err(anyhow::anyhow!("SMTP is not configured")),
            },
        }
        .unwrap_or_else(|error| {
            eprintln!("agents: outbox UID {uid} to {to}: {error:#}");
            Submission::Temporary
        });
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
        delivered = false;
        eprintln!("agents: delivery failed for outbox UID {uid}; see mail pending");
    }
    Ok(delivered)
}

fn deliver_local(
    target: &Workspace,
    from: &MailAddress,
    to: &MailAddress,
    message: &[u8],
) -> Result<Submission> {
    let mut inbox = target
        .mail()
        .open(target.root(), target.mail_directory(), false)?;
    inbox.append(None, &stamp(to, from, message)?)?;
    Ok(Submission::Accepted)
}

/// Removes finished outbox records older than `retry.retain`.
pub fn compact(workspace: &Workspace) -> Result<()> {
    let mail = workspace.mail();
    if mail.retry.retain == 0 {
        return Ok(());
    }
    let mut store = mail.open(workspace.root(), workspace.mail_directory(), true)?;
    let now = retry::now()?;
    let mut old = Vec::new();
    for state in TERMINAL {
        for uid in store.search(&Query {
            required: &[state],
            ..Query::default()
        })? {
            if let Some(raw) = store.message(uid)?
                && retry::QueueEntry::read(&raw)
                    .is_ok_and(|entry| now.saturating_sub(entry.created) >= mail.retry.retain)
            {
                old.push(uid);
            }
        }
    }
    old.sort_unstable();
    old.dedup();
    store.expunge(&old)
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

pub(super) fn stamp(to: &MailAddress, from: &MailAddress, raw: &[u8]) -> Result<Vec<u8>> {
    let message = MessageParser::default()
        .parse(raw)
        .context("invalid RFC 5322 mail")?;
    for name in [LOCAL_TO, LOCAL_FROM] {
        ensure!(
            unique_header(&message, name)?.is_none(),
            "message already contains {name}"
        );
    }
    let mut stamped = format!("{LOCAL_TO}: {to}\r\n{LOCAL_FROM}: {from}\r\n").into_bytes();
    stamped.extend_from_slice(raw);
    Ok(stamped)
}

/// Every record in the outbox is queued until it reaches a terminal state.
const READY_QUERY: Query<'static> = Query {
    unseen: false,
    required: &[],
    excluded: TERMINAL,
    header: None,
    after_uid: 0,
};

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
    for other in store.search(&Query {
        header: Some(("Message-ID", id)),
        after_uid: uid,
        ..Query::default()
    })? {
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
