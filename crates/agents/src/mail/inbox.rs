use super::{
    AddressPolicy,
    request::{GetAttachment, ListMail, MailRef, SearchMail},
    store::{Folder, Query},
};
use crate::types::MailAddress;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use mail_parser::{Address, HeaderValue, Message, MessageParser, MimeHeaders};
use serde_json::{Value, json};

pub(super) const LOCAL_TO: &str = "X-Agents-Local-To";
pub(super) const LOCAL_FROM: &str = "X-Agents-Local-From";
pub(super) const UNSIGNED: &str = "X-Agents-Local-Unsigned";
/// How the sender was authenticated, such as `openpgp` or `dkim example.com`.
pub(super) const AUTHENTICATION: &str = "X-Agents-Local-Authentication";
pub(super) const INGESTED: &str = "X-Agents-Local-Imported";
/// The daemon started an agent run for this message.
pub(super) const DISPATCHED: &str = "AgentsDispatched";
/// The agent run for this message failed or timed out.
pub(super) const FAILED: &str = "AgentsFailed";

pub struct Inbox<'a> {
    folder: Folder,
    address: MailAddress,
    validity: u32,
    incoming: &'a AddressPolicy,
    allow_unsigned: bool,
}

pub struct Reply {
    pub sender: MailAddress,
    pub to: Vec<MailAddress>,
    pub cc: Vec<MailAddress>,
    pub subject: String,
    pub parent: String,
    pub references: String,
}

pub struct MailEvent {
    pub(super) mail: MailRef,
    pub(super) sender: String,
    pub(super) ids: Vec<String>,
}

impl MailEvent {
    pub fn keys(&self) -> Vec<String> {
        if self.ids.is_empty() {
            return vec![format!(
                "{}\nuid:{}:{}",
                self.sender, self.mail.uid_validity, self.mail.uid
            )];
        }
        self.ids
            .iter()
            .map(|id| super::sessions::message_key(&self.sender, id))
            .collect()
    }
}

impl<'a> Inbox<'a> {
    pub(super) fn open(
        folder: Folder,
        address: MailAddress,
        incoming: &'a AddressPolicy,
        allow_unsigned: bool,
    ) -> Result<Self> {
        let validity = folder.validity()?;
        Ok(Self {
            folder,
            address,
            validity,
            incoming,
            allow_unsigned,
        })
    }

    fn visible(&self, message: &Message<'_>) -> bool {
        unique_header(message, LOCAL_TO)
            .ok()
            .flatten()
            .and_then(|value| value.parse::<MailAddress>().ok())
            .is_some_and(|address| address.matches(&self.address))
            && (self.allow_unsigned || !is_unsigned(message))
            && routing_sender(message).is_ok_and(|sender| self.incoming.allows(&sender, true))
    }

    /// Newest first.
    fn candidates(&mut self, query: Query<'_>, before: Option<u32>) -> Result<Vec<u32>> {
        let mut uids = self.folder.search(&Query {
            header: Some((LOCAL_TO, &self.address.to_string())),
            ..query
        })?;
        uids.retain(|uid| before.is_none_or(|before| *uid < before));
        uids.reverse();
        Ok(uids)
    }

    fn unread(unread: bool) -> Query<'static> {
        Query {
            unseen: unread,
            ..Query::default()
        }
    }

    pub fn list(&mut self, request: ListMail) -> Result<Value> {
        let uids = self.candidates(Self::unread(request.unread_only), request.before_uid)?;
        self.page(uids, request.limit, None)
    }

    /// Unread visible mail that no agent run was started for, oldest first.
    pub fn pending_runs(&mut self) -> Result<Vec<MailRef>> {
        let mut pending = Vec::new();
        let query = Query {
            unseen: true,
            excluded: &[DISPATCHED],
            ..Query::default()
        };
        for uid in self.candidates(query, None)?.into_iter().rev() {
            if let Some(raw) = self.folder.message(uid)?
                && let Some(message) = MessageParser::default().parse(&raw)
                && self.visible(&message)
            {
                pending.push(MailRef {
                    uid,
                    uid_validity: self.validity,
                });
            }
        }
        Ok(pending)
    }

    /// Records that a run started, finished, or should be repeated.
    pub fn mark(&mut self, id: MailRef, keyword: &str, add: bool) -> Result<()> {
        self.inspect(id, |this, _| this.folder.keyword(id.uid, keyword, add))
    }

    pub fn event(&mut self, mail: MailRef) -> Result<MailEvent> {
        self.inspect(mail, |_, message| {
            let mut ids = message_ids(message.references());
            ids.extend(message_ids(message.in_reply_to()));
            ids.extend(unique_header(message, "Message-ID")?.and_then(super::outbox::message_id));
            ids.sort();
            ids.dedup();
            Ok(MailEvent {
                mail,
                sender: routing_sender(message)?.routing_key(),
                ids,
            })
        })
    }

    pub fn search(&mut self, request: SearchMail) -> Result<Value> {
        ensure!(!request.query.trim().is_empty(), "search query is empty");
        // The inbox is local: search decoded message headers and bodies.
        let uids = self.candidates(Self::unread(request.unread_only), request.before_uid)?;
        self.page(uids, request.limit, Some(&request.query.to_lowercase()))
    }

    fn page(
        &mut self,
        uids: Vec<u32>,
        limit: Option<usize>,
        contains: Option<&str>,
    ) -> Result<Value> {
        let limit = limit.unwrap_or(20);
        ensure!(limit > 0, "limit must be positive");
        let mut items = Vec::new();
        let mut cursor = None;
        for uid in uids {
            let Some(raw) = self.folder.message(uid)? else {
                continue;
            };
            let Some(message) = MessageParser::default().parse(&raw) else {
                continue;
            };
            if !self.visible(&message) {
                continue;
            }
            let body = text(&message);
            if contains.is_some_and(|query| {
                !body.to_lowercase().contains(query)
                    && !message.headers().iter().any(|header| {
                        header
                            .value
                            .as_text()
                            .is_some_and(|value| value.to_lowercase().contains(query))
                    })
                    && !message.headers_raw().any(|(name, value)| {
                        format!("{name}: {value}").to_lowercase().contains(query)
                    })
            }) {
                continue;
            }
            if items.len() == limit {
                cursor = items.last().map(|(uid, _)| *uid);
                break;
            }
            items.push((uid, self.summary(uid, &message, &body)?));
        }
        Ok(json!({
            "messages": items.into_iter().map(|(_, item)| item).collect::<Vec<_>>(),
            "next_before_uid": cursor
        }))
    }

    fn inspect<T>(
        &mut self,
        id: MailRef,
        read: impl FnOnce(&mut Self, &Message<'_>) -> Result<T>,
    ) -> Result<T> {
        ensure!(
            id.uid_validity == self.validity && id.uid != 0,
            "stale or invalid mail reference"
        );
        let raw = self
            .folder
            .message(id.uid)?
            .context("mail is unavailable")?;
        let message = MessageParser::default()
            .parse(&raw)
            .context("mail is unavailable")?;
        ensure!(self.visible(&message), "mail is unavailable");
        read(self, &message)
    }

    fn summary(&mut self, uid: u32, message: &Message<'_>, body: &str) -> Result<Value> {
        let summary = body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect::<String>();
        let flags = self.folder.flags(uid)?;
        let has = |name: &str| flags.iter().any(|flag| flag.eq_ignore_ascii_case(name));
        Ok(json!({
            "mail": MailRef { uid, uid_validity: self.validity },
            "from": unique_header(message, LOCAL_FROM).ok().flatten(),
            "authenticated": !is_unsigned(message),
            "authentication": unique_header(message, AUTHENTICATION).ok().flatten(),
            "subject": message.subject(),
            "date": message.date().map(|date| date.to_rfc3339()),
            "unread": !has("\\Seen"),
            "dispatched": has(DISPATCHED),
            "failed": has(FAILED),
            "summary": summary
        }))
    }

    pub fn get(&mut self, id: MailRef) -> Result<Value> {
        self.inspect(id, |this, message| this.full(id.uid, message))
    }

    pub fn reply(&mut self, id: MailRef, all: bool) -> Result<Reply> {
        self.inspect(id, |_, message| {
            let sender = routing_sender(message)?;
            let reply_to_count = message
                .headers()
                .iter()
                .filter(|header| header.name.as_str().eq_ignore_ascii_case("Reply-To"))
                .count();
            ensure!(reply_to_count <= 1, "duplicate Reply-To header");
            let to = if reply_to_count == 1 {
                addresses(message.reply_to()).context("invalid Reply-To")?
            } else {
                addresses(message.from()).context("invalid From")?
            };
            ensure!(!to.is_empty(), "reply header does not contain a mailbox");
            let mut cc = Vec::new();
            if all {
                cc.extend(addresses(message.to()).context("invalid original To")?);
                cc.extend(addresses(message.cc()).context("invalid original Cc")?);
            }
            let parent = unique_header(message, "Message-ID")?
                .and_then(super::outbox::message_id)
                .context("cannot reply without a valid Message-ID")?;
            let mut references = message_ids(message.references());
            if references.is_empty() {
                let inherited = message_ids(message.in_reply_to());
                if inherited.len() == 1 {
                    references = inherited;
                }
            }
            if references.last() != Some(&parent) {
                references.push(parent.clone());
            }
            let subject = message.subject().unwrap_or("");
            let subject = if subject
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"))
            {
                subject.to_owned()
            } else {
                format!("Re: {subject}")
            };
            Ok(Reply {
                sender,
                to,
                cc,
                subject,
                parent: format!("<{parent}>"),
                references: references
                    .iter()
                    .map(|id| format!("<{id}>"))
                    .collect::<Vec<_>>()
                    .join(" "),
            })
        })
    }

    fn full(&mut self, uid: u32, message: &Message<'_>) -> Result<Value> {
        let body = text(message);
        let summary = self.summary(uid, message, &body)?;
        let attachments: Vec<_> = message.attachments().enumerate().map(|(index, part)| {
            json!({"index":index,"name":part.attachment_name(),"size":part.contents().len()})
        }).collect();
        Ok(json!({
            "message": summary, "body": body, "message_id": message.message_id(),
            "headers": message.headers_raw().collect::<Vec<_>>(), "attachments": attachments
        }))
    }

    pub fn attachment(&mut self, request: GetAttachment) -> Result<Value> {
        self.inspect(request.mail, |_, message| {
            let part = message
                .attachments()
                .nth(request.index)
                .context("attachment not found")?;
            Ok(json!({
                "name": part.attachment_name(),
                "content_base64": STANDARD.encode(part.contents())
            }))
        })
    }

    pub fn set_read(&mut self, id: MailRef, read: bool) -> Result<Value> {
        self.inspect(id, |this, _| this.folder.set_read(id.uid, read))?;
        Ok(json!({"read":read}))
    }

    pub fn thread(&mut self, id: MailRef) -> Result<Value> {
        self.inspect(id, |this, message| {
            let Some(root) = thread_root(message) else {
                return Ok(json!({"messages":[this.full(id.uid, message)?]}));
            };
            let mut found = Vec::new();
            for uid in this.candidates(Query::default(), None)?.into_iter().rev() {
                let Some(raw) = this.folder.message(uid)? else {
                    continue;
                };
                let Some(other) = MessageParser::default().parse(&raw) else {
                    continue;
                };
                if this.visible(&other) && thread_root(&other) == Some(root) {
                    found.push(this.full(uid, &other)?);
                }
            }
            Ok(json!({"messages":found}))
        })
    }
}

pub(super) fn addresses(header: Option<&Address<'_>>) -> Result<Vec<MailAddress>> {
    header
        .into_iter()
        .flat_map(Address::iter)
        .map(|entry| {
            entry
                .address()
                .context("missing mailbox address")?
                .parse()
                .map_err(anyhow::Error::msg)
        })
        .collect()
}

pub fn unique_header<'a>(message: &'a Message<'_>, name: &str) -> Result<Option<&'a str>> {
    let mut values = message
        .headers()
        .iter()
        .filter(|header| header.name.as_str().eq_ignore_ascii_case(name));
    let Some(first) = values.next() else {
        return Ok(None);
    };
    ensure!(values.next().is_none(), "duplicate {name} header");
    Ok(Some(
        first.value.as_text().context("invalid mail header")?.trim(),
    ))
}

pub(super) fn routing_sender(message: &Message<'_>) -> Result<MailAddress> {
    unique_header(message, LOCAL_FROM)?
        .context("mail has no routing sender")?
        .parse()
        .map_err(anyhow::Error::msg)
}

fn is_unsigned(message: &Message<'_>) -> bool {
    matches!(unique_header(message, UNSIGNED), Ok(Some("yes")))
}

fn thread_root<'a>(message: &'a Message<'_>) -> Option<&'a str> {
    let first = |value: &'a HeaderValue<'_>| {
        value
            .as_text_list()
            .and_then(|list| list.first().map(|id| id.as_ref()))
            .or_else(|| value.as_text())
    };
    first(message.references())
        .or_else(|| first(message.in_reply_to()))
        .or_else(|| message.message_id())
        .map(|id| id.trim_matches(['<', '>']))
}

pub(super) fn message_ids(value: &HeaderValue<'_>) -> Vec<String> {
    if let Some(ids) = value.as_text_list() {
        ids.iter()
            .filter_map(|id| super::outbox::message_id(id))
            .collect()
    } else {
        value
            .as_text()
            .and_then(super::outbox::message_id)
            .into_iter()
            .collect()
    }
}

fn text<'a>(message: &'a Message<'a>) -> String {
    message
        .text_bodies()
        .enumerate()
        .filter_map(|(index, _)| message.body_text(index))
        .collect::<Vec<_>>()
        .join("\n\n")
}
