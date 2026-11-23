mod layers;
mod live;
mod routing;
pub mod schema;
pub mod values;

use crate::{
    harness::Harness,
    types::{Id, Program, RelativePath, Version},
};
use anyhow::{Context, Result, bail, ensure};
use ignore::gitignore::GitignoreBuilder;
pub use live::{Live, Snapshot};
use routing::{AgentMail, ProjectMail};
pub use routing::{AgentMailbox, MailRoutes};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use values::References;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    #[serde(rename = "version")]
    _version: Version,
    /// Agent to use unless one is explicitly selected.
    agent: Id,
    /// Project instructions. Defaults to the contents of AGENTS.md if present.
    instructions: Option<String>,
    /// Harness adapters available to this project.
    #[serde(default)]
    harnesses: BTreeMap<Id, Harness>,
    /// MCP server definitions available to this project.
    #[serde(default)]
    mcp: BTreeMap<Id, McpDefinition>,
    /// Agent definitions keyed by name.
    agents: BTreeMap<Id, InlineAgent>,
    /// Override skill entrypoint paths relative to the project root.
    #[serde(default)]
    skills: BTreeMap<Id, RelativePath>,
    #[serde(default)]
    mail: ProjectMail,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InlineAgent {
    harness: Id,
    model: Option<String>,
    description: String,
    instructions: String,
    /// Ordered MCP selection patterns: select on a match, exclude with `!`.
    #[serde(default)]
    mcp: Vec<String>,
    /// Skills to expose to this agent. Omit to discover project skills; [] disables them.
    #[serde(default, deserialize_with = "optional_unique_ids")]
    skills: Option<BTreeSet<Id>>,
    mail: Option<AgentMail>,
    /// Seconds before a mail-triggered run and its child processes are killed.
    #[schemars(range(min = 1))]
    timeout: Option<u64>,
}

fn optional_unique_ids<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BTreeSet<Id>>, D::Error> {
    let Some(list) = Option::<Vec<Id>>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let mut set = BTreeSet::new();
    for id in list {
        if set.contains(&id) {
            return Err(serde::de::Error::custom(format!(
                "duplicate identifier {id:?}"
            )));
        }
        set.insert(id);
    }
    Ok(Some(set))
}

#[derive(Serialize)]
pub struct Agent {
    pub id: Id,
    description: String,
    instructions: String,
}

#[derive(Clone, Copy, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Http,
    Sse,
    Ws,
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum McpDefinition {
    Disabled {
        #[serde(rename = "disable")]
        _disable: Disabled,
    },
    Enabled(Mcp),
}

struct Disabled;

impl<'de> Deserialize<'de> for Disabled {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("expected disable: true"))
        }
    }
}

impl JsonSchema for Disabled {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        "McpDisabled".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "boolean", "const": true})
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Mcp {
    Stdio(StdioMcp),
    Remote(RemoteMcp),
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StdioMcp {
    #[serde(default, rename = "type")]
    transport: StdioTransport,
    #[serde(default, rename = "disable", skip_serializing)]
    _disable: bool,
    command: Program,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum StdioTransport {
    #[default]
    Stdio,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteMcp {
    #[serde(rename = "type")]
    transport: RemoteTransport,
    #[serde(default, rename = "disable", skip_serializing)]
    _disable: bool,
    url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RemoteTransport {
    Http,
    #[serde(rename(deserialize = "streamable-http", serialize = "http"))]
    StreamableHttp,
    Sse,
    Ws,
}

impl Mcp {
    pub fn stdio(command: Program, args: Vec<String>) -> Self {
        Self::Stdio(StdioMcp {
            transport: StdioTransport::Stdio,
            _disable: false,
            command,
            args,
            env: BTreeMap::new(),
            cwd: None,
        })
    }

    pub fn transport(&self) -> Transport {
        match self {
            Self::Stdio(_) => Transport::Stdio,
            Self::Remote(server) => match server.transport {
                RemoteTransport::Http | RemoteTransport::StreamableHttp => Transport::Http,
                RemoteTransport::Sse => Transport::Sse,
                RemoteTransport::Ws => Transport::Ws,
            },
        }
    }
}

pub struct Project {
    pub root: PathBuf,
    pub harness: Harness,
    pub harness_id: Id,
    pub model: Option<String>,
    pub agent: Agent,
    pub project_instructions: Option<String>,
    pub skills: BTreeMap<Id, PathBuf>,
    pub mcp: BTreeMap<Id, Mcp>,
    pub references: Arc<References>,
    /// Limit for mail-triggered runs; unset means the default.
    pub timeout: Option<Duration>,
}

pub struct Workspace {
    selected: Arc<LocatedProject>,
}

pub struct LocatedProject {
    pub root: PathBuf,
    config: ProjectConfig,
    references: Arc<References>,
    sources: layers::Sources,
}

impl LocatedProject {
    fn find(root: &Path) -> Result<Option<Self>> {
        let Some((config, references, sources)) = layers::load::<ProjectConfig>(root)? else {
            return Ok(None);
        };
        ensure!(
            config.agents.contains_key(&config.agent),
            "default agent {} is not defined in {}",
            config.agent,
            root.display()
        );
        config
            .mail
            .transport
            .check()
            .context("invalid mail transport configuration")?;
        for (id, agent) in &config.agents {
            ensure!(
                agent.timeout != Some(0),
                "agents.{id}.timeout must be positive"
            );
        }
        Ok(Some(Self {
            root: root.to_owned(),
            config,
            references: Arc::new(references),
            sources,
        }))
    }
}

impl Workspace {
    pub fn root(&self) -> &Path {
        &self.selected.root
    }

    pub fn load(directory: Option<&Path>) -> Result<Self> {
        let start = directory.map_or_else(env::current_dir, fs::canonicalize)?;
        ensure!(start.is_dir(), "{} is not a directory", start.display());
        Ok(Self {
            selected: Arc::new(discover(&start)?),
        })
    }

    pub fn project(&self, agent: Option<&Id>) -> Result<Project> {
        let located = &self.selected;
        let agent_id = agent.unwrap_or(&located.config.agent);
        Self::resolve_project(located, agent_id).map_err(|error| {
            // Errors may quote resolved secrets; do not reveal them.
            if located.references.used {
                anyhow::anyhow!("could not resolve agent configuration with external references")
            } else {
                error
            }
        })
    }

    fn resolve_project(located: &LocatedProject, agent_id: &Id) -> Result<Project> {
        let project = &located.config;
        let root = located.root.clone();
        let definition = project
            .agents
            .get(agent_id)
            .with_context(|| format!("undefined agent {agent_id}"))?;
        let harness_id = &definition.harness;
        let harness = project
            .harnesses
            .get(harness_id)
            .cloned()
            .with_context(|| {
                format!(
                    "agent {agent_id} uses unregistered harness {harness_id}; \
                     define harnesses.{harness_id} in the project configuration"
                )
            })?;
        let agent = Agent {
            id: agent_id.clone(),
            description: definition.description.clone(),
            instructions: definition.instructions.clone(),
        };
        let project_instructions = match &project.instructions {
            Some(instructions) => Some(instructions.clone()),
            None => match fs::symlink_metadata(root.join("AGENTS.md")) {
                Ok(_) => Some(read_text(&inside(&root, Path::new("AGENTS.md"))?)?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("inspect project AGENTS.md"),
            },
        };
        let names = match &definition.skills {
            Some(names) => names.clone(),
            None => discover_skills(&root)?
                .into_iter()
                .chain(project.skills.keys().cloned())
                .collect(),
        };
        let mut skills = BTreeMap::new();
        for name in names {
            let path = if let Some(reference) = project.skills.get(&name) {
                inside(&root, reference.as_ref())?
            } else {
                let definitions = inside(&root, Path::new(".agents"))?;
                inside(
                    &definitions,
                    &Path::new("skills").join(&name).join("SKILL.md"),
                )?
            };
            ensure!(path.is_file(), "{} is not a skill file", path.display());
            skills.insert(name, path);
        }
        let mcp = select_mcp(&project.mcp, &definition.mcp)?;
        Ok(Project {
            root,
            harness,
            harness_id: harness_id.clone(),
            model: definition.model.clone(),
            agent,
            project_instructions,
            skills,
            mcp,
            references: Arc::clone(&located.references),
            timeout: definition.timeout.map(Duration::from_secs),
        })
    }
}

fn discover_skills(root: &Path) -> Result<BTreeSet<Id>> {
    let relative = Path::new(".agents/skills");
    match fs::symlink_metadata(root.join(relative)) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).context("inspect project skills directory"),
    }
    let definitions = inside(root, Path::new(".agents"))?;
    let directory = inside(&definitions, Path::new("skills"))?;
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&directory).context("discover project skills")? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<Id>().ok())
        else {
            continue;
        };
        match fs::symlink_metadata(entry.path().join("SKILL.md")) {
            Ok(metadata) if metadata.is_file() || metadata.is_symlink() => {
                names.insert(name);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect discovered SKILL.md"),
        }
    }
    Ok(names)
}

fn select_mcp(
    servers: &BTreeMap<Id, McpDefinition>,
    patterns: &[String],
) -> Result<BTreeMap<Id, Mcp>> {
    let mut rules = GitignoreBuilder::new(".");
    for pattern in patterns {
        ensure!(
            !pattern.contains(['\r', '\n']),
            "MCP patterns must be single lines"
        );
        if let Ok(name) = pattern.parse::<Id>() {
            ensure!(servers.contains_key(&name), "unknown MCP {pattern:?}");
        }
        rules
            .add_line(None, pattern)
            .with_context(|| format!("invalid MCP pattern {pattern:?}"))?;
    }
    let rules = rules.build().context("compile MCP patterns")?;
    Ok(servers
        .iter()
        .filter_map(|(name, definition)| match definition {
            McpDefinition::Enabled(server) if rules.matched(name, false).is_ignore() => {
                Some((name.clone(), server.clone()))
            }
            _ => None,
        })
        .collect())
}

fn discover(cwd: &Path) -> Result<LocatedProject> {
    for path in cwd.ancestors() {
        if let Some(project) = LocatedProject::find(path)? {
            return Ok(project);
        }
    }
    bail!(
        "no .agents.json, .agents.json5, .agents.jsonc, .agents.yaml, .agents.yml or .agents.d layers found at or above {}",
        cwd.display()
    )
}

pub fn inside(root: &Path, relative: &Path) -> Result<PathBuf> {
    let path = root.join(relative);
    let resolved = fs::canonicalize(&path).with_context(|| format!("locate {}", path.display()))?;
    ensure!(
        resolved.starts_with(root),
        "{} escapes {}",
        path.display(),
        root.display()
    );
    Ok(resolved)
}

pub fn read_text(path: &Path) -> Result<String> {
    ensure!(path.is_file(), "{} is not a regular file", path.display());
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}
