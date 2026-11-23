use crate::config::{Project, Transport};
use crate::types::{EnvName, FileName, Program, Version};
use anyhow::{Context, Result, ensure};
use minijinja::{
    AutoEscape, Environment, Error, ErrorKind, UndefinedBehavior, Value, context,
    value::{ValueKind, ViaDeserialize},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, BufRead, BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    #[serde(rename = "version")]
    _version: Version,
    program: Program,
    args: Vec<Arg>,
    /// Child-only environment overrides; null removes an inherited value.
    #[serde(default)]
    env: BTreeMap<EnvName, Option<String>>,
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default)]
    files: BTreeMap<FileName, Artifact>,
    stdin: Option<String>,
    /// Read a native session ID from a JSONL event on stdout when starting a mail session.
    session_id_event: Option<SessionIdEvent>,
    /// Arguments for a command that must succeed before resuming a mail session.
    session_resume_check: Option<Vec<Arg>>,
}

impl Harness {
    pub fn supports_sessions(&self) -> bool {
        self.capabilities.sessions
    }
}

#[derive(Serialize)]
pub struct Session {
    pub id: String,
    pub resume: bool,
    pub directory: PathBuf,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SessionIdEvent {
    /// Expected value of the JSON event's `type` field.
    event: String,
    /// JSON field containing the native session ID.
    field: String,
    /// Require a UUID rather than accepting an arbitrary identifier.
    #[serde(default)]
    uuid: bool,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Arg {
    Inline(String),
    File(ArgTemplate),
    Conditional(ArgGroup),
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ArgTemplate {
    template: String,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ArgGroup {
    /// MiniJinja expression that must evaluate to a boolean.
    when: String,
    /// Arguments emitted at this position when the condition is true.
    args: Vec<Arg>,
}

fn render_args(
    args: &[Arg],
    env: &Environment<'_>,
    ctx: &Value,
    output: &mut Vec<String>,
) -> Result<()> {
    for (index, arg) in args.iter().enumerate() {
        let rendered = (|| -> Result<Option<String>> {
            Ok(match arg {
                Arg::Inline(template) => Some(env.render_str(template, ctx.clone())?),
                Arg::File(file) => Some(env.render_str(&file.template, ctx.clone())?),
                Arg::Conditional(group) => {
                    let include = env.compile_expression(&group.when)?.eval(ctx.clone())?;
                    ensure!(
                        include.kind() == ValueKind::Bool,
                        "`when` must evaluate to a boolean"
                    );
                    if include.is_true() {
                        render_args(&group.args, env, ctx, output)?;
                    }
                    None
                }
            })
        })()
        .with_context(|| format!("render args[{index}]"))?;
        if let Some(arg) = rendered {
            ensure!(!arg.contains('\0'), "args[{index}] produced a NUL byte");
            output.push(arg);
        }
    }
    Ok(())
}

#[derive(Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    #[serde(default)]
    mcp: Vec<Transport>,
    #[serde(default)]
    skills: bool,
    /// Can create and resume a persistent native session using the supplied session ID.
    #[serde(default)]
    sessions: bool,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Artifact {
    template: String,
    format: Format,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Format {
    Text,
    Json,
}

#[derive(Serialize)]
pub struct Plan {
    cwd: PathBuf,
    program: Program,
    args: Vec<String>,
    child_env: BTreeMap<EnvName, Option<String>>,
    files: BTreeMap<FileName, String>,
    stdin_template: Option<String>,
    #[serde(skip)]
    env: Environment<'static>,
    #[serde(skip)]
    reference_env: BTreeSet<EnvName>,
    #[serde(skip)]
    session_id_event: Option<SessionIdEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resume_check: Option<Vec<String>>,
}

impl Plan {
    pub fn render(project: &Project, run_dir: &Path) -> Result<Self> {
        Self::render_with_session(project, run_dir, None)
    }

    pub fn render_with_session(
        project: &Project,
        run_dir: &Path,
        session: Option<&Session>,
    ) -> Result<Self> {
        Self::render_resolved(project, run_dir, session).map_err(|error| {
            if project.references.used {
                anyhow::anyhow!("could not render harness configuration with external references")
            } else {
                error
            }
        })
    }

    fn render_resolved(
        project: &Project,
        run_dir: &Path,
        session: Option<&Session>,
    ) -> Result<Self> {
        let harness = &project.harness;
        for (name, server) in &project.mcp {
            ensure!(
                harness.capabilities.mcp.contains(&server.transport()),
                "harness does not support the transport for MCP {name:?}"
            );
        }
        ensure!(
            project.skills.is_empty() || harness.capabilities.skills,
            "harness does not support declared skills"
        );
        let mut paths = BTreeMap::new();
        for name in harness.files.keys() {
            let file = run_dir.join(name);
            paths.insert(
                name,
                file.to_str()
                    .context("run directory path is not UTF-8")?
                    .to_owned(),
            );
        }
        let skills = project
            .skills
            .iter()
            .map(|(name, path)| Ok((name, path.to_str().context("skill path is not UTF-8")?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let ctx = context! {
            agent => &project.agent,
            project_instructions => &project.project_instructions,
            mcp => &project.mcp,
            skills => skills,
            paths => paths,
            model => &project.model,
            session => session,
        };
        let mut env = templates();
        let mut files = BTreeMap::new();
        for (name, artifact) in &harness.files {
            let content = env
                .render_str(&artifact.template, ctx.clone())
                .with_context(|| format!("render {name}"))?;
            if matches!(artifact.format, Format::Json) {
                serde_json::from_str::<serde_json::Value>(&content)
                    .with_context(|| format!("generated {name} is not valid JSON"))?;
            }
            files.insert(name.clone(), content);
        }
        let mut args = Vec::new();
        render_args(&harness.args, &env, &ctx, &mut args)?;
        let resume_check = harness
            .session_resume_check
            .as_ref()
            .filter(|_| session.is_some_and(|session| session.resume))
            .map(|check| {
                let mut args = Vec::new();
                render_args(check, &env, &ctx, &mut args)?;
                ensure!(!args.is_empty(), "empty session resume check");
                Ok::<_, anyhow::Error>(args)
            })
            .transpose()?;
        let session_id_event = harness
            .session_id_event
            .clone()
            .filter(|_| session.is_some_and(|session| !session.resume));
        let mut child_env = BTreeMap::new();
        for (name, template) in &harness.env {
            let value = template
                .as_deref()
                .map(|template| env.render_str(template, ctx.clone()))
                .transpose()
                .with_context(|| format!("render child environment variable {name}"))?;
            ensure!(
                value.as_ref().is_none_or(|value| !value.contains('\0')),
                "child environment variable {name} contains a NUL byte"
            );
            child_env.insert(name.clone(), value);
        }
        let stdin_template = harness.stdin.clone();
        if let Some(template) = &stdin_template {
            env.add_template_owned("stdin", template.clone())
                .context("compile stdin template")?;
        }
        Ok(Self {
            cwd: project.root.clone(),
            program: harness.program.clone(),
            args,
            child_env,
            files,
            stdin_template,
            env,
            reference_env: project.references.env.clone(),
            session_id_event,
            resume_check,
        })
    }

    pub fn execute(
        self,
        run_dir: &Path,
        task_input: impl Read,
        secrets: &[&EnvName],
    ) -> Result<ExitStatus> {
        self.execute_observing(run_dir, task_input, secrets, |_| {})
    }

    pub fn captures_session_id(&self) -> bool {
        self.session_id_event.is_some()
    }

    pub fn execute_observing(
        self,
        run_dir: &Path,
        mut task_input: impl Read,
        secrets: &[&EnvName],
        mut observe: impl FnMut(String),
    ) -> Result<ExitStatus> {
        for (name, content) in self.files {
            fs::write(run_dir.join(&name), content).with_context(|| format!("write {name}"))?;
        }
        let mut input = tempfile::tempfile_in(run_dir).context("create task spool")?;
        if self.stdin_template.is_some() {
            let task = io::read_to_string(&mut task_input).context("read UTF-8 task")?;
            let prompt = self
                .env
                .get_template("stdin")?
                .render(context! { task })
                .context("render stdin template")?;
            input.write_all(prompt.as_bytes())?;
        } else {
            io::copy(&mut task_input, &mut input).context("spool task")?;
        }
        input.rewind()?;
        let command = |args: &[String]| {
            let mut command = Command::new(&self.program);
            command.current_dir(&self.cwd).args(args);
            for name in &self.reference_env {
                command.env_remove(name);
            }
            for (name, value) in &self.child_env {
                match value {
                    Some(value) => command.env(name, value),
                    None => command.env_remove(name),
                };
            }
            for secret in secrets {
                command.env_remove(secret);
            }
            command
        };
        if let Some(check) = &self.resume_check {
            let status = command(check)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .context("check original harness session")?;
            ensure!(
                status.success(),
                "original harness session is unavailable ({status})"
            );
        }
        let mut command = command(&self.args);
        command.stdin(Stdio::from(input));
        let Some(event) = self.session_id_event else {
            return command.status().context("run harness");
        };
        let mut child = command
            .stdout(Stdio::piped())
            .spawn()
            .context("run harness")?;
        let mut pipe = BufReader::new(child.stdout.take().expect("piped harness stdout"));
        let result = relay_session_events(&mut pipe, &event, &mut observe);
        drop(pipe);
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().context("wait for harness")?;
        result?;
        Ok(status)
    }
}

fn relay_session_events(
    pipe: &mut impl BufRead,
    event: &SessionIdEvent,
    observe: &mut impl FnMut(String),
) -> Result<()> {
    let mut line = Vec::new();
    let mut found = false;
    while pipe.read_until(b'\n', &mut line)? != 0 {
        if !found
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line)
            && value["type"].as_str() == Some(event.event.as_str())
            && let Some(id) = value.get(&event.field).and_then(serde_json::Value::as_str)
            && valid_native_id(id)
            && (!event.uuid || uuid::Uuid::parse_str(id).is_ok())
        {
            found = true;
            observe(id.to_owned());
        }
        io::stdout().lock().write_all(&line)?;
        line.clear();
    }
    Ok(())
}

fn valid_native_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn templates() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_keep_trailing_newline(true);
    env.add_filter(
        "totoml",
        |ViaDeserialize(value): ViaDeserialize<serde_json::Value>| {
            toml::Value::try_from(value)
                .map(|value| value.to_string())
                .map_err(|error| {
                    Error::new(
                        ErrorKind::InvalidOperation,
                        format!("encode TOML value: {error}"),
                    )
                })
        },
    );
    env
}
