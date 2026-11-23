use super::{LocatedProject, Workspace};
use crate::{
    mail::MailConfig,
    template::{Renderer, Template},
    types::{AgentSelector, Id, MailAddress, RelativePath},
};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectMail {
    /// Optional root for discovering mail identities, relative to this project.
    workspace: Option<PathBuf>,
    /// Default address template for agents in this project.
    address: Option<Template>,
    /// Where local mbox files are stored, relative to the project.
    directory: Option<RelativePath>,
    #[serde(default)]
    pub(super) transport: MailConfig,
    /// Limit incoming mail from registered agents. Omission allows all agents.
    from: Option<Vec<AgentSelector>>,
    /// Limit outgoing mail to registered agents. Omission allows all agents.
    to: Option<Vec<AgentSelector>>,
    /// Human mailboxes trusted as authenticated senders and reply recipients.
    trust: Option<Vec<MailAddress>>,
    /// Monitoring recipients copied on all outgoing mail.
    #[serde(default)]
    cc: Vec<MailAddress>,
    /// Monitoring recipients blind-copied on all outgoing mail.
    #[serde(default)]
    bcc: Vec<MailAddress>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentMail {
    /// Address template overriding the project default.
    address: Option<Template>,
    from: Option<Vec<AgentSelector>>,
    to: Option<Vec<AgentSelector>>,
}

pub struct MailRoutes {
    root: PathBuf,
    entries: BTreeMap<String, AgentMailbox>,
    /// Paths whose metadata changes when the routing configuration may have changed.
    pub(super) watched: Vec<PathBuf>,
    /// Digests of every routed project's merged configuration.
    pub(super) digests: Vec<[u8; 32]>,
}

impl MailRoutes {
    pub fn route(&self, recipient: &MailAddress) -> Result<&AgentMailbox> {
        self.entries
            .get(&recipient.routing_key())
            .with_context(|| format!("no agent registered for {recipient}"))
    }

    pub fn identities(&self) -> Vec<MailIdentity<'_>> {
        self.entries
            .values()
            .map(|mailbox| MailIdentity {
                project: &mailbox.project.root,
                agent: &mailbox.agent,
                address: &mailbox.address,
            })
            .collect()
    }

    fn agent(&self, root: &Path, agent: &Id) -> Option<&AgentMailbox> {
        self.entries
            .values()
            .find(|mailbox| mailbox.project.root == root && &mailbox.agent == agent)
    }

    pub fn project_mailboxes(&self, root: &Path) -> impl Iterator<Item = (&Id, &AgentMailbox)> {
        self.entries
            .values()
            .filter(move |mailbox| mailbox.project.root == root)
            .map(|mailbox| (&mailbox.agent, mailbox))
    }

    pub fn peers(&self, source: &AgentMailbox) -> Result<BTreeMap<String, MailAddress>> {
        self.entries
            .values()
            .map(|target| {
                let selector = if source.project.root == target.project.root {
                    target.agent.to_string()
                } else {
                    let path = target
                        .project
                        .root
                        .strip_prefix(&self.root)
                        .expect("registered mail workspace");
                    let path = if path.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        path
                    };
                    format!(
                        "{}:{}",
                        path.to_str().context("target project path is not UTF-8")?,
                        target.agent
                    )
                };
                Ok((selector, target.address.clone()))
            })
            .collect()
    }
}

#[derive(Serialize)]
pub struct MailIdentity<'a> {
    project: &'a Path,
    agent: &'a Id,
    address: &'a MailAddress,
}

#[derive(Clone)]
pub struct AgentMailbox {
    pub project: Arc<LocatedProject>,
    pub agent: Id,
    address: MailAddress,
}

impl AgentMailbox {
    pub fn address(&self) -> &MailAddress {
        &self.address
    }

    pub fn from(&self) -> [Option<&[AgentSelector]>; 2] {
        [
            self.project.config.mail.from.as_deref(),
            self.mail().and_then(|mail| mail.from.as_deref()),
        ]
    }

    pub fn to(&self) -> [Option<&[AgentSelector]>; 2] {
        [
            self.project.config.mail.to.as_deref(),
            self.mail().and_then(|mail| mail.to.as_deref()),
        ]
    }

    pub fn trust(&self) -> Option<&[MailAddress]> {
        self.project.config.mail.trust.as_deref()
    }

    pub fn copies(&self) -> (&[MailAddress], &[MailAddress]) {
        (&self.project.config.mail.cc, &self.project.config.mail.bcc)
    }

    pub fn mail_directory(&self) -> &Path {
        self.project.mail_directory()
    }

    fn mail(&self) -> Option<&AgentMail> {
        self.project.config.agents.get(&self.agent)?.mail.as_ref()
    }
}

impl LocatedProject {
    fn mail_directory(&self) -> &Path {
        self.config
            .mail
            .directory
            .as_ref()
            .map_or(Path::new(".agents/mail"), AsRef::as_ref)
    }
}

impl Workspace {
    pub fn at_mailbox(mailbox: &AgentMailbox) -> Self {
        Self {
            selected: Arc::clone(&mailbox.project),
        }
    }

    pub fn mail(&self) -> &MailConfig {
        &self.selected.config.mail.transport
    }

    pub fn mail_directory(&self) -> &Path {
        self.selected.mail_directory()
    }

    pub fn mail_routes(&self) -> Result<MailRoutes> {
        let project = &self.selected;
        let root = match &project.config.mail.workspace {
            Some(path) => {
                fs::canonicalize(project.root.join(path)).context("locate mail workspace")?
            }
            None => project.root.clone(),
        };
        ensure!(
            project.root.starts_with(&root),
            "project is outside the mail workspace"
        );
        self.routes_from(root)
    }

    fn routes_from(&self, root: PathBuf) -> Result<MailRoutes> {
        let mut routes = MailRoutes {
            root: root.clone(),
            entries: BTreeMap::new(),
            watched: Vec::new(),
            digests: Vec::new(),
        };
        let mut directories = vec![root.clone()];
        while let Some(directory) = directories.pop() {
            let entry = if self.selected.root == directory {
                Some(Arc::clone(&self.selected))
            } else {
                LocatedProject::find(&directory)?.map(Arc::new)
            };
            if self.selected.config.mail.workspace.is_some() {
                routes.watched.push(directory.clone());
            }
            if let Some(project) = entry {
                routes.watched.extend(project.sources.paths.iter().cloned());
                routes.digests.push(project.sources.digest);
                let default = project.config.mail.address.as_ref();
                let name = project
                    .root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let relative = project
                    .root
                    .strip_prefix(&root)
                    .context("project outside mail workspace")?;
                let path = if relative.as_os_str().is_empty() {
                    ".".into()
                } else {
                    relative.to_string_lossy()
                };
                for (agent, definition) in &project.config.agents {
                    if definition.mail.is_none() && default.is_none() {
                        continue;
                    }
                    let source =
                        || format!("mail address for {agent} in {}", project.root.display());
                    let template = definition
                        .mail
                        .as_ref()
                        .and_then(|mail| mail.address.as_ref())
                        .or(default)
                        .with_context(|| format!("no {}", source()))?;
                    let context = serde_json::json!({
                        "project": {"name": name, "path": path},
                        "agent": {"id": agent},
                    });
                    let address = Renderer::new(context)?
                        .string(template)
                        .with_context(|| format!("render {}", source()))?
                        .parse::<MailAddress>()
                        .map_err(anyhow::Error::msg)
                        .with_context(|| format!("invalid {}", source()))?;
                    let key = address.routing_key();
                    ensure!(
                        !routes.entries.contains_key(&key),
                        "multiple agents registered for {}",
                        address
                    );
                    routes.entries.insert(
                        key,
                        AgentMailbox {
                            project: Arc::clone(&project),
                            agent: agent.clone(),
                            address,
                        },
                    );
                }
            }
            if self.selected.config.mail.workspace.is_none() {
                break;
            }
            for entry in directory
                .read_dir()
                .with_context(|| format!("read {}", directory.display()))?
            {
                let entry = entry?;
                if entry.file_name() != ".agents"
                    && entry.file_name() != ".agents.d"
                    && entry.file_name() != ".git"
                    && entry.file_type()?.is_dir()
                {
                    directories.push(entry.path());
                }
            }
        }
        Ok(routes)
    }

    pub fn mailbox_for(&self, agent: Option<&Id>) -> Result<(MailRoutes, AgentMailbox)> {
        let routes = self.mail_routes()?;
        let agent = agent.unwrap_or(&self.selected.config.agent);
        let mailbox = routes
            .agent(&self.selected.root, agent)
            .with_context(|| format!("no mail address configured for agent {agent}"))?
            .clone();
        Ok((routes, mailbox))
    }
}
