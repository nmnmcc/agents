use super::print_json;
use crate::{
    config::Workspace,
    mail::{
        self, MailAccess,
        request::{GetAttachment, ListMail, MailRef, Request, SearchMail, SendMail, SetRead},
    },
    types::{Id, MailAddress},
};
use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;
use std::{
    io::{self, Read},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Args)]
pub struct Identity {
    /// Use this agent instead of the project's default.
    #[arg(long)]
    agent: Option<Id>,
}

#[derive(Args)]
pub struct Page {
    /// Show only unread mail.
    #[arg(long)]
    unread_only: bool,
    /// Continue before this UID, using next_before_uid from the last page.
    #[arg(long)]
    before_uid: Option<u32>,
    /// Maximum messages to return (default: 20).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Args)]
pub struct Reference {
    /// Message UID returned by list or search.
    uid: u32,
    /// Mailbox UID validity returned alongside the UID.
    #[arg(long)]
    uid_validity: u32,
}

impl From<Reference> for MailRef {
    fn from(reference: Reference) -> Self {
        Self {
            uid: reference.uid,
            uid_validity: reference.uid_validity,
        }
    }
}

#[derive(Clone, ValueEnum)]
pub enum ReadState {
    Read,
    Unread,
}

#[derive(Args)]
pub struct Recipients {
    /// Add an agent ID, workspace-relative path:agent, or trusted human address.
    #[arg(long, value_name = "AGENT")]
    to: Vec<String>,
    /// Add a Cc recipient.
    #[arg(long, value_name = "AGENT")]
    cc: Vec<String>,
    /// Add a blind recipient without exposing their address in the message.
    #[arg(long, value_name = "AGENT")]
    bcc: Vec<String>,
}

impl Recipients {
    fn request(
        self,
        in_reply_to: Option<MailRef>,
        subject: Option<String>,
        reply_all: bool,
    ) -> Result<Request> {
        let mut body = String::new();
        io::stdin().lock().read_to_string(&mut body)?;
        Ok(Request::Send(SendMail {
            to: self.to,
            cc: self.cc,
            bcc: self.bcc,
            in_reply_to,
            reply_all,
            subject,
            body,
        }))
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// List mail, newest first.
    List {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        page: Page,
    },
    /// Search message headers and bodies.
    Search {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        page: Page,
        query: String,
    },
    /// Read a message without marking it read.
    Get {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        reference: Reference,
    },
    /// Read visible inbox messages in the same thread.
    Thread {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        reference: Reference,
    },
    /// Return an attachment as base64 in JSON.
    Attachment {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        reference: Reference,
        /// Zero-based attachment index.
        index: usize,
    },
    /// Mark a message read or unread.
    SetRead {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        reference: Reference,
        state: ReadState,
    },
    /// Queue mail to another agent, with the body read from stdin.
    Send {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        recipients: Recipients,
        #[arg(long)]
        subject: String,
    },
    /// Reply in-thread to Reply-To or From; read the body from stdin.
    Reply {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        reference: Reference,
        #[command(flatten)]
        recipients: Recipients,
        /// Override the original subject.
        #[arg(long)]
        subject: Option<String>,
        /// Include authorized original To and Cc recipients.
        #[arg(long)]
        all: bool,
    },
}

impl Command {
    pub fn run(self, workspace: &Workspace) -> Result<Value> {
        let (identity, request) = match self {
            Self::List { identity, page } => (
                identity,
                Request::List(ListMail {
                    unread_only: page.unread_only,
                    before_uid: page.before_uid,
                    limit: page.limit,
                }),
            ),
            Self::Search {
                identity,
                page,
                query,
            } => (
                identity,
                Request::Search(SearchMail {
                    query,
                    unread_only: page.unread_only,
                    before_uid: page.before_uid,
                    limit: page.limit,
                }),
            ),
            Self::Get {
                identity,
                reference,
            } => (identity, Request::Get(reference.into())),
            Self::Thread {
                identity,
                reference,
            } => (identity, Request::Thread(reference.into())),
            Self::Attachment {
                identity,
                reference,
                index,
            } => (
                identity,
                Request::Attachment(GetAttachment {
                    mail: reference.into(),
                    index,
                }),
            ),
            Self::SetRead {
                identity,
                reference,
                state,
            } => (
                identity,
                Request::SetRead(SetRead {
                    mail: reference.into(),
                    read: matches!(state, ReadState::Read),
                }),
            ),
            Self::Send {
                identity,
                recipients,
                subject,
            } => (identity, recipients.request(None, Some(subject), false)?),
            Self::Reply {
                identity,
                reference,
                recipients,
                subject,
                all,
            } => (
                identity,
                recipients.request(Some(reference.into()), subject, all)?,
            ),
        };
        let mail = workspace.mail();
        let (routes, mailbox) = workspace.mailbox_for(identity.agent.as_ref())?;
        MailAccess::new(mail, &routes, &mailbox)?.handle(request)
    }
}

#[derive(Subcommand)]
pub(super) enum Action {
    #[command(flatten)]
    Access(Command),
    /// List resolved agent addresses without connecting to mail servers.
    Addresses,
    /// Deliver queued mail locally or through SMTP without launching an agent.
    Flush,
    /// List pending, expired or rejected deliveries.
    Pending,
    /// Import unseen IMAP mail without launching an agent.
    Receive,
    /// Clear a message's dispatched and failed state so the daemon runs the agent for it again.
    Retry {
        #[command(flatten)]
        identity: Identity,
        #[command(flatten)]
        reference: Reference,
    },
    /// Verify and import an RFC 5322 message on stdin for each project agent in To and Cc.
    Deliver {
        /// Deliver only to this agent, for example the envelope recipient given by an MDA.
        #[arg(long)]
        recipient: Option<MailAddress>,
        /// Import an unsigned message; requires mail.transport.dangerously_allow_unsigned.
        #[arg(long)]
        unsigned: bool,
    },
}

impl Action {
    pub(super) fn run(self, directory: Option<PathBuf>) -> Result<ExitCode> {
        let workspace = Workspace::load(directory.as_deref())?;
        match self {
            Self::Access(command) => {
                print_json(&command.run(&workspace)?)?;
                Ok(ExitCode::SUCCESS)
            }
            Self::Addresses => {
                print_json(&workspace.mail_routes()?.identities())?;
                Ok(ExitCode::SUCCESS)
            }
            Self::Flush => Ok(if mail::flush(&workspace)? {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }),
            Self::Pending => {
                print_json(&mail::pending(&workspace)?)?;
                Ok(ExitCode::SUCCESS)
            }
            Self::Receive => {
                mail::ImapIngress::default().receive(&workspace, &workspace.mail_routes()?)?;
                Ok(ExitCode::SUCCESS)
            }
            Self::Retry {
                identity,
                reference,
            } => {
                let (routes, mailbox) = workspace.mailbox_for(identity.agent.as_ref())?;
                MailAccess::new(workspace.mail(), &routes, &mailbox)?.retry(reference.into())?;
                Ok(ExitCode::SUCCESS)
            }
            Self::Deliver {
                recipient,
                unsigned,
            } => {
                let mut raw = Vec::new();
                io::stdin()
                    .lock()
                    .take(8 * 1024 * 1024 + 1)
                    .read_to_end(&mut raw)?;
                mail::deliver(&workspace, recipient.as_ref(), unsigned, &raw)?;
                Ok(ExitCode::SUCCESS)
            }
        }
    }
}
