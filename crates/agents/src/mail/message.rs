use crate::types::{Id, MailAddress};
use anyhow::{Context, Result};
use mail_parser::{MessageParser, MimeHeaders, PartType};
use serde::Serialize;
use std::{io::Read, path::Path};

#[derive(Serialize)]
pub struct Preview<'a> {
    pub project: &'a Path,
    pub agent: &'a Id,
    pub recipient: &'a MailAddress,
    #[serde(flatten)]
    pub task: Task,
}

#[derive(Serialize)]
pub struct Task {
    pub subject: Option<String>,
    pub message_id: Option<String>,
    pub body: String,
}

impl Task {
    pub fn read(mut input: impl Read) -> Result<Self> {
        let mut raw = Vec::new();
        input
            .read_to_end(&mut raw)
            .context("read RFC 5322 message")?;
        let message = MessageParser::default()
            .parse(&raw)
            .context("could not parse RFC 5322 message")?;
        Self::from_message(&message)
    }

    pub fn from_message(message: &mail_parser::Message<'_>) -> Result<Self> {
        let body = message
            .text_bodies()
            .filter(|part| {
                (part.content_type().is_none() || part.is_content_type("text", "plain"))
                    && !part
                        .content_disposition()
                        .is_some_and(|kind| kind.is_attachment())
            })
            .find_map(|part| match &part.body {
                PartType::Text(body) if !body.trim().is_empty() => Some(body.to_string()),
                _ => None,
            })
            .context("mail has no nonempty text/plain body")?;
        Ok(Self {
            subject: message.subject().map(str::to_owned),
            message_id: message.message_id().map(str::to_owned),
            body,
        })
    }
}
