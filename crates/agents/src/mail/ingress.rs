use super::{
    MailAccess, compose,
    inbox::{INGESTED, LOCAL_TO, UNSIGNED, authenticated_sender, unique_header},
    outbox::{message_id, stamp},
    signatures::{LIMIT, Verified},
    store, transport,
};
use crate::{
    config::{MailRoutes, Workspace},
    types::MailAddress,
};
use anyhow::{Context, Result, ensure};
use mail_parser::{MessageParser, MimeHeaders, PartType};
use std::{collections::BTreeSet, fs::OpenOptions};

#[derive(Default)]
pub struct ImapIngress {
    validity: Option<u32>,
    last: u32,
}

impl ImapIngress {
    pub fn poll(&mut self, workspace: &Workspace, routes: &MailRoutes) -> Result<()> {
        let Some(config) = &workspace.mail().imap else {
            return Ok(());
        };
        let mut imap = transport::connect(config, workspace.root())?;
        let validity = imap.select(&config.mailbox, &[])?;
        if self.validity != Some(validity) {
            self.last = 0;
            self.validity = Some(validity);
        }
        let query = format!("UNDELETED UNSEEN UID {}:*", self.last.saturating_add(1));
        for uid in imap.search(&query)? {
            if uid <= self.last {
                continue;
            }
            if let Some(raw) = imap.message_prefix(uid, LIMIT + 1)? {
                let id = format!("imap-{validity}-{uid}@unsigned.invalid");
                let mail = workspace.mail();
                if let Some(task) = mail.signatures.verify(workspace.root(), &raw)? {
                    import(workspace, routes, task, false)?;
                } else if mail.allow_unsigned
                    && let Some(task) = unsigned_task(workspace, routes, None, &raw, id)?
                {
                    import(workspace, routes, task, true)?;
                }
            }
            self.last = uid;
        }
        Ok(())
    }
}

pub fn deliver(
    workspace: &Workspace,
    recipient: Option<&MailAddress>,
    unsigned: bool,
    raw: &[u8],
) -> Result<()> {
    ensure!(raw.len() <= LIMIT, "mail exceeds size limit");
    let routes = workspace.mail_routes()?;
    let mail = workspace.mail();
    let task = if unsigned {
        ensure!(mail.allow_unsigned, "unsigned mail ingress is disabled");
        let recipient = recipient.context("--unsigned requires --recipient")?;
        let id = format!("{}@unsigned.invalid", uuid::Uuid::new_v4());
        unsigned_task(workspace, &routes, Some(recipient), raw, id)?
            .context("not a supported unsigned mail message")?
    } else {
        mail.signatures
            .verify(workspace.root(), raw)?
            .context("mail has no valid trusted signature and protected task")?
    };
    ensure!(
        recipient.is_none_or(|address| address.matches(&task.recipient)),
        "mail recipient mismatch"
    );
    ensure!(
        import(workspace, &routes, task, unsigned)?,
        "mail recipient or sender is not permitted"
    );
    Ok(())
}

fn import(
    workspace: &Workspace,
    routes: &MailRoutes,
    task: Verified,
    unsigned: bool,
) -> Result<bool> {
    let Ok(mailbox) = routes.route(&task.recipient) else {
        return Ok(false);
    };
    if mailbox.project.root != workspace.root() {
        return Ok(false);
    }
    let access = MailAccess::new(workspace.mail(), routes, mailbox)?;
    if !unsigned && !access.incoming.allows(&task.sender, true) {
        return Ok(false);
    }
    let lock = store::prepare(workspace.root(), workspace.mail_directory(), "import.lock")?;
    let lock = OpenOptions::new().write(true).open(lock)?;
    lock.lock()?;
    let mut inbox = workspace
        .mail()
        .open(workspace.root(), workspace.mail_directory(), false)?;
    // The durable receipt and the task are the same record; there is no second
    // write to lose on a crash. Hold the lock across lookup and append.
    for uid in inbox.search("ALL", &[], &[])? {
        if let Some(raw) = inbox.message(uid)?
            && let Some(message) = MessageParser::default().parse(&raw)
            && matches!(unique_header(&message, INGESTED), Ok(Some("yes")))
            && message.message_id().and_then(message_id).as_deref() == Some(task.id.as_str())
            && authenticated_sender(&message).is_ok_and(|sender| sender.matches(&task.sender))
            && unique_header(&message, LOCAL_TO)
                .ok()
                .flatten()
                .and_then(|value| value.parse::<MailAddress>().ok())
                .is_some_and(|address| address.matches(&task.recipient))
        {
            return Ok(true);
        }
    }
    let mut record = format!("{INGESTED}: yes\r\n").into_bytes();
    if unsigned {
        record.extend_from_slice(format!("{UNSIGNED}: yes\r\n").as_bytes());
    }
    record.extend_from_slice(&stamp(&task.recipient, &task.sender, &task.message)?);
    inbox.append(None, &record)?;
    Ok(true)
}

fn unsigned_task(
    workspace: &Workspace,
    routes: &MailRoutes,
    recipient: Option<&MailAddress>,
    raw: &[u8],
    id: String,
) -> Result<Option<Verified>> {
    if raw.len() > LIMIT {
        return Ok(None);
    }
    let Some(message) = MessageParser::default().parse(raw) else {
        return Ok(None);
    };
    // Do not reinterpret an invalid signed or encrypted message as unsigned.
    if message.parts.iter().any(|part| {
        part.content_type().is_some_and(|ty| {
            let subtype = ty.subtype().unwrap_or_default().to_ascii_lowercase();
            subtype.contains("pkcs7")
                || matches!(
                    subtype.as_str(),
                    "signed" | "encrypted" | "pgp-signature" | "pgp-encrypted"
                )
        }) || matches!(&part.body, PartType::Text(text) if text.contains("-----BEGIN PGP "))
    }) {
        return Ok(None);
    }
    let recipient = if let Some(recipient) = recipient {
        recipient.clone()
    } else {
        let mut seen = BTreeSet::new();
        let matched = [message.to(), message.cc()]
            .into_iter()
            .flatten()
            .flat_map(|addresses| addresses.iter())
            .filter_map(|entry| entry.address()?.parse::<MailAddress>().ok())
            .filter(|address| {
                routes
                    .route(address)
                    .is_ok_and(|m| m.project.root == workspace.root())
            })
            .filter(|address| seen.insert(address.routing_key()))
            .collect::<Vec<_>>();
        let [recipient] = matched.as_slice() else {
            return Ok(None);
        };
        recipient.clone()
    };
    let Some(body) = message.body_text(0).filter(|body| !body.trim().is_empty()) else {
        return Ok(None);
    };
    let sender = "anonymous@unsigned.invalid"
        .parse::<MailAddress>()
        .expect("valid placeholder address");
    let message = compose(
        &sender,
        std::slice::from_ref(&recipient),
        &[],
        &recipient,
        message.subject().unwrap_or("Unsigned agent task"),
        body.into_owned(),
        None,
        &id,
    )?;
    Ok(Some(Verified {
        sender,
        recipient,
        id,
        message,
    }))
}
