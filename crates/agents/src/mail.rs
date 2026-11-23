mod inbox;
pub mod mcp;
mod outbox;
pub mod request;
mod retry;
mod run;
mod sessions;
mod store;
mod transport;

use crate::{
    config::{AgentMailbox, MailRoutes},
    types::{AgentSelector, EnvName, MailAddress},
};
use anyhow::{Context, Result, ensure};
use inbox::{Inbox, Reply};
use lettre::{
    Message,
    address::Envelope,
    message::{
        SinglePart,
        header::{HeaderName, HeaderValue},
    },
};
use request::{MailRef, Request, SendMail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sessions::Sessions;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use store::Folder;

pub use outbox::{deliver, flush, pending};
pub use run::execute;
pub use sessions::SessionBusy;

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailConfig {
    /// Dedicated mailbox written only by a gateway that replaces the envelope recipient stamp.
    imap: Option<transport::Incoming>,
    /// An IMAP outbox folder, defaulting to AgentOutbox when IMAP is configured.
    outbox: Option<String>,
    smtp: Option<transport::Outgoing>,
    #[serde(default, deserialize_with = "retry::decode")]
    retry: retry::Policy,
}

impl MailConfig {
    fn open(&self, root: &Path, directory: &Path, outbox: bool) -> Result<Folder> {
        let Some(incoming) = &self.imap else {
            return Folder::local(
                root,
                directory,
                if outbox { "outbox.mbox" } else { "inbox.mbox" },
            );
        };
        let outbox_name = self.outbox.as_deref().unwrap_or("AgentOutbox");
        ensure!(
            !outbox_name.eq_ignore_ascii_case(&incoming.mailbox),
            "outbox must be a separate IMAP folder"
        );
        let (name, keywords) = if outbox {
            (outbox_name, outbox::FLAGS)
        } else {
            (incoming.mailbox.as_str(), &[][..])
        };
        Folder::imap(transport::connect(incoming, root)?, name, keywords)
    }

    fn secrets(&self) -> Vec<&EnvName> {
        let incoming = self.imap.as_ref().map(|incoming| &incoming.login);
        let outgoing = self
            .smtp
            .as_ref()
            .and_then(|outgoing| outgoing.login.as_ref());
        incoming
            .into_iter()
            .chain(outgoing)
            .filter_map(transport::Login::password_env)
            .collect()
    }
}

pub struct MailAccess<'a> {
    mail: &'a MailConfig,
    root: PathBuf,
    directory: PathBuf,
    address: MailAddress,
    peers: BTreeMap<String, MailAddress>,
    incoming: AddressPolicy,
    outgoing: AddressPolicy,
    cc: Vec<MailAddress>,
    bcc: Vec<MailAddress>,
}

struct AddressPolicy {
    agents: BTreeSet<String>,
    allowed_agents: BTreeSet<String>,
    trusted_humans: Option<BTreeSet<String>>,
}

impl AddressPolicy {
    fn new(
        peers: &BTreeMap<String, MailAddress>,
        rules: [Option<&[AgentSelector]>; 2],
        trust: Option<&[MailAddress]>,
    ) -> Self {
        let allowed_agents = peers
            .iter()
            .filter(|(name, _)| {
                rules
                    .iter()
                    .flatten()
                    .all(|list| list.iter().any(|id| id.as_str() == *name))
            })
            .map(|(_, address)| address.routing_key())
            .collect();
        Self {
            agents: peers.values().map(MailAddress::routing_key).collect(),
            allowed_agents,
            trusted_humans: trust.map(|list| list.iter().map(MailAddress::routing_key).collect()),
        }
    }

    fn allows(&self, address: &MailAddress, default_human: bool) -> bool {
        let key = address.routing_key();
        if self.agents.contains(&key) {
            self.allowed_agents.contains(&key)
        } else {
            self.trusted_humans
                .as_ref()
                .map_or(default_human, |list| list.contains(&key))
        }
    }
}

impl<'a> MailAccess<'a> {
    pub fn new(mail: &'a MailConfig, routes: &MailRoutes, mailbox: &AgentMailbox) -> Result<Self> {
        let address = mailbox.address().clone();
        let peers = routes.peers(mailbox)?;
        let incoming = AddressPolicy::new(&peers, mailbox.from(), mailbox.trust());
        let outgoing = AddressPolicy::new(&peers, mailbox.to(), mailbox.trust());
        let (cc, bcc) = mailbox.copies();
        Ok(Self {
            mail,
            root: mailbox.project.root.clone(),
            directory: mailbox.mail_directory().to_owned(),
            address,
            peers,
            incoming,
            outgoing,
            cc: cc.to_vec(),
            bcc: bcc.to_vec(),
        })
    }

    pub fn unread_refs(&self) -> Result<BTreeSet<MailRef>> {
        self.inbox()?.unread_refs()
    }

    fn inbox(&self) -> Result<Inbox<'_>> {
        Inbox::open(
            self.mail.open(&self.root, &self.directory, false)?,
            self.address.clone(),
            &self.incoming,
        )
    }

    pub fn handle(&self, request: Request) -> Result<Value> {
        match request {
            Request::List(request) => self.inbox()?.list(request),
            Request::Search(request) => self.inbox()?.search(request),
            Request::Get(id) => self.inbox()?.get(id),
            Request::Thread(id) => self.inbox()?.thread(id),
            Request::Attachment(request) => self.inbox()?.attachment(request),
            Request::SetRead(request) => self.inbox()?.set_read(request.mail, request.read),
            Request::Send(request) => self.send(request, None),
        }
    }

    fn send(&self, request: SendMail, sessions: Option<&mut Sessions>) -> Result<Value> {
        ensure!(!request.body.trim().is_empty(), "message body is empty");
        ensure!(
            !request.reply_all || request.in_reply_to.is_some(),
            "reply_all requires in_reply_to"
        );
        let reply = request
            .in_reply_to
            .map(|id| self.inbox()?.reply(id, request.reply_all))
            .transpose()?;
        let subject = request
            .subject
            .as_deref()
            .or_else(|| reply.as_ref().map(|r| r.subject.as_str()))
            .context("subject is required for new mail")?;
        let (mut to, mut cc, mut recipients) = (Vec::new(), Vec::new(), Vec::new());
        if let Some(reply) = &reply {
            for (group, header) in [(&mut to, &reply.to), (&mut cc, &reply.cc)] {
                for address in header {
                    if self.address.matches(address) {
                        continue;
                    }
                    ensure!(
                        self.outgoing
                            .allows(address, reply.authenticated_sender.matches(address)),
                        "reply recipient is not authorized: {address}"
                    );
                    if add_recipient(&mut recipients, address) {
                        group.push(address.clone());
                    }
                }
            }
        }
        let mut resolve = |selectors: Vec<String>| -> Result<Vec<MailAddress>> {
            let mut addresses = Vec::new();
            for selector in selectors {
                let address = if let Some(peer) = self.peers.get(&selector) {
                    peer.clone()
                } else {
                    let address: MailAddress = selector.parse().map_err(|_| {
                        anyhow::anyhow!("unknown or unapproved target agent: {selector}")
                    })?;
                    ensure!(
                        !self.outgoing.agents.contains(&address.routing_key()),
                        "use the agent ID for registered mailboxes: {selector}"
                    );
                    address
                };
                ensure!(
                    self.outgoing.allows(&address, false),
                    "unapproved recipient: {selector}"
                );
                if add_recipient(&mut recipients, &address) {
                    addresses.push(address);
                }
            }
            Ok(addresses)
        };
        to.extend(resolve(request.to)?);
        cc.extend(resolve(request.cc)?);
        resolve(request.bcc)?;
        ensure!(!recipients.is_empty(), "at least one recipient is required");
        for address in &self.cc {
            if !self.bcc.iter().any(|blind| blind.matches(address))
                && add_recipient(&mut recipients, address)
            {
                cc.push(address.clone());
            }
        }
        for address in &self.bcc {
            add_recipient(&mut recipients, address);
        }
        let from = self.address.to_string();
        let domain = from.rsplit_once('@').context("missing route domain")?.1;
        let id = format!("{}@{domain}", uuid::Uuid::new_v4());
        let message = compose(
            &self.address,
            &to,
            &cc,
            &recipients[0],
            subject,
            request.body,
            reply.as_ref(),
            &id,
        )?;
        if let Some(sessions) = sessions {
            let mut keys = request
                .in_reply_to
                .map(|reference| sessions.reply_keys(&self.inbox()?.event(reference)?))
                .transpose()?
                .unwrap_or_default();
            keys.extend(
                recipients
                    .iter()
                    .map(|recipient| sessions::message_key(&recipient.routing_key(), &id)),
            );
            // Persist the routing before a queued message can be delivered.
            sessions.bind_outgoing(keys)?;
        }
        let mut queue = self.mail.open(&self.root, &self.directory, true)?;
        for (queued, recipient) in recipients.iter().enumerate() {
            outbox::enqueue(&mut queue, &message, recipient).with_context(|| {
                format!(
                    "could not confirm queueing recipient {}; {queued} previously confirmed",
                    queued + 1
                )
            })?;
        }
        Ok(json!({"queued":true,"recipients":recipients.len()}))
    }
}

fn add_recipient(recipients: &mut Vec<MailAddress>, address: &MailAddress) -> bool {
    if recipients.iter().any(|existing| existing.matches(address)) {
        return false;
    }
    recipients.push(address.clone());
    true
}

#[allow(clippy::too_many_arguments)]
fn compose(
    from: &MailAddress,
    to: &[MailAddress],
    cc: &[MailAddress],
    first: &MailAddress,
    subject: &str,
    body: String,
    reply: Option<&Reply>,
    id: &str,
) -> Result<Vec<u8>> {
    let from = from.to_string();
    let mut builder = Message::builder()
        .from(from.parse()?)
        // The actual SMTP envelope is supplied by the outbox per recipient.
        .envelope(Envelope::new(
            Some(from.parse()?),
            vec![first.to_string().parse()?],
        )?)
        .subject(subject.replace(['\r', '\n', '\0'], " "))
        .message_id(Some(format!("<{id}>")))
        .raw_header(HeaderValue::new(
            HeaderName::new_from_ascii_str("X-Agents-Kind"),
            "task".into(),
        ));
    for address in to {
        builder = builder.to(address.to_string().parse()?);
    }
    for address in cc {
        builder = builder.cc(address.to_string().parse()?);
    }
    if let Some(reply) = reply {
        builder = builder
            .in_reply_to(reply.parent.clone())
            .references(reply.references.clone());
    }
    let message = builder.singlepart(SinglePart::plain(body))?;
    Ok(message.formatted())
}
