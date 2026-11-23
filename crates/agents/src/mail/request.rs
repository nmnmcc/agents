use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMail {
    /// Single target agent (shorthand for one entry in to).
    pub agent: Option<String>,
    /// Agent IDs, workspace-relative path:agent selectors, or trusted human email addresses.
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    /// Reply to this inbox message's Reply-To or From, subject to authorization.
    pub in_reply_to: Option<MailRef>,
    /// Include the original visible To and Cc recipients when replying.
    #[serde(default)]
    pub reply_all: bool,
    /// Required for new mail; defaults to the original subject for replies.
    pub subject: Option<String>,
    pub body: String,
}

#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct MailRef {
    pub uid_validity: u32,
    pub uid: u32,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListMail {
    #[serde(default)]
    pub unread_only: bool,
    pub before_uid: Option<u32>,
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchMail {
    /// Text to search in message headers and bodies.
    pub query: String,
    #[serde(default)]
    pub unread_only: bool,
    pub before_uid: Option<u32>,
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetRead {
    pub mail: MailRef,
    pub read: bool,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAttachment {
    pub mail: MailRef,
    pub index: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "method", content = "arguments", rename_all = "snake_case")]
pub enum Request {
    Send(SendMail),
    List(ListMail),
    Search(SearchMail),
    Get(MailRef),
    Thread(MailRef),
    Attachment(GetAttachment),
    SetRead(SetRead),
}
