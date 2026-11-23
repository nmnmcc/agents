use crate::config::{Project, Transport};
use crate::template::{Renderer, Sandbox, Script, Template};
use crate::types::{EnvName, FileName, Program};
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    program: Program,
    args: Vec<Template>,
    #[serde(default)]
    env: Env,
    #[serde(default)]
    capabilities: Capabilities,
    #[serde(default)]
    files: BTreeMap<FileName, Artifact>,
    /// Renders the harness input from `task`.
    stdin: Option<Template>,
    #[serde(default)]
    hooks: Hooks,
}

/// Child-only environment overrides: a map whose `null` values remove inherited
/// variables, or a script that returns such a map.
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
enum Env {
    Js(Script),
    Variables(BTreeMap<EnvName, Option<Template>>),
}

impl Default for Env {
    fn default() -> Self {
        Self::Variables(BTreeMap::new())
    }
}

impl Env {
    fn render(&self, renderer: &Renderer) -> Result<BTreeMap<EnvName, Option<String>>> {
        let variables = match self {
            Self::Js(script) => {
                let value = renderer.script(script).context("render env")?;
                serde_json::from_value::<BTreeMap<EnvName, Option<String>>>(value).context(
                    "env script must return an object mapping variable names to strings or null",
                )?
            }
            Self::Variables(templates) => templates
                .iter()
                .map(|(name, template)| {
                    let value = match template {
                        None => None,
                        Some(template) => match renderer.value(template) {
                            Ok(Value::Null) => None,
                            Ok(Value::String(value)) => Some(value),
                            Ok(other) => bail!(
                                "render environment variable {name}: expected a string or null, got {other}"
                            ),
                            Err(error) => {
                                return Err(error.context(format!("render environment variable {name}")));
                            }
                        },
                    };
                    Ok((name.clone(), value))
                })
                .collect::<Result<_>>()?,
        };
        for (name, value) in &variables {
            ensure!(
                value.as_ref().is_none_or(|value| !value.contains('\0')),
                "environment variable {name} contains a NUL byte"
            );
        }
        Ok(variables)
    }
}

impl Harness {
    pub fn supports_sessions(&self) -> bool {
        self.capabilities.sessions != Sessions::None
    }
}

/// Programs and scripts around one harness run. Commands run in the project
/// directory with the harness environment; each is an argument list whose
/// first element is the program, and an empty list is skipped.
#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Hooks {
    /// Commands run in order before the harness starts; any failure aborts the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    before: Vec<Vec<Template>>,
    /// Commands run in order after the harness exits, with its exit code as
    /// `status` (null if killed by a signal); any failure fails the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    after: Vec<Vec<Template>>,
    /// Script run on each line of harness stdout.
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<Script>,
    /// Script run on each line of harness stderr.
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<Script>,
}

#[derive(Serialize)]
pub struct Session {
    pub id: String,
    pub resume: bool,
}

/// Renders arguments in order. A string becomes one argument; JavaScript may
/// also return an array of arguments, or null for none.
fn render_args(args: &[Template], renderer: &Renderer) -> Result<Vec<String>> {
    let mut output = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        (|| -> Result<()> {
            match renderer.value(arg)? {
                Value::Null => {}
                Value::String(arg) => output.push(arg),
                Value::Array(items) => {
                    for item in items {
                        let Value::String(arg) = item else {
                            bail!("expected an array of strings, got {item}");
                        };
                        output.push(arg);
                    }
                }
                other => bail!("expected a string, array, or null, got {other}"),
            }
            ensure!(
                output.iter().all(|arg| !arg.contains('\0')),
                "produced a NUL byte"
            );
            Ok(())
        })()
        .with_context(|| format!("render args[{index}]"))?;
    }
    Ok(output)
}

#[derive(Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    #[serde(default)]
    mcp: Vec<Transport>,
    #[serde(default)]
    skills: bool,
    #[serde(default)]
    sessions: Sessions,
}

/// How the harness identifies persistent native sessions.
#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Sessions {
    /// No persistent sessions.
    #[default]
    None,
    /// Creates and resumes sessions under the ID `agents` supplies.
    Supplied,
    /// Assigns its own ID, which an output hook reports for a new session.
    Reported,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Artifact {
    /// JavaScript may return any JSON value for a `json` file.
    template: Template,
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
    stdin_template: Option<Template>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    before: Vec<Vec<String>>,
    hooks: Hooks,
    #[serde(skip)]
    context: Value,
    #[serde(skip)]
    reports_session: bool,
    #[serde(skip)]
    reference_env: BTreeSet<EnvName>,
}

/// What an output hook asked for one line.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LineAction {
    /// Replacement text; null drops the line; absent keeps it.
    #[serde(default, deserialize_with = "present")]
    line: Option<Option<String>>,
    /// Native session ID reported by the harness.
    session: Option<String>,
}

fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

impl Plan {
    pub fn render(project: &Project, run_dir: &Path, session: Option<&Session>) -> Result<Self> {
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
        let reported = harness.capabilities.sessions == Sessions::Reported;
        ensure!(
            !reported || harness.hooks.stdout.is_some() || harness.hooks.stderr.is_some(),
            "reported sessions require a stdout or stderr hook"
        );
        let context = json!({
            "program": &harness.program,
            "agent": &project.agent,
            "project_instructions": &project.project_instructions,
            "mcp": &project.mcp,
            "skills": skills,
            "paths": paths,
            "model": &project.model,
            "session": session,
        });
        let renderer = Renderer::new(&context)?;
        let mut files = BTreeMap::new();
        for (name, artifact) in &harness.files {
            let content = (|| -> Result<String> {
                Ok(
                    match (renderer.value(&artifact.template)?, &artifact.format) {
                        (Value::String(content), Format::Json) => {
                            serde_json::from_str::<Value>(&content)
                                .context("generated content is not valid JSON")?;
                            content
                        }
                        (Value::String(content), Format::Text) => content,
                        (value, Format::Json) => serde_json::to_string_pretty(&value)?,
                        (value, Format::Text) => bail!("expected a string, got {value}"),
                    },
                )
            })()
            .with_context(|| format!("render {name}"))?;
            files.insert(name.clone(), content);
        }
        let args = render_args(&harness.args, &renderer)?;
        let before = harness
            .hooks
            .before
            .iter()
            .enumerate()
            .map(|(index, hook)| {
                render_args(hook, &renderer)
                    .with_context(|| format!("render hooks.before[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?;
        let child_env = harness.env.render(&renderer)?;
        Ok(Self {
            cwd: project.root.clone(),
            program: harness.program.clone(),
            args,
            child_env,
            files,
            stdin_template: harness.stdin.clone(),
            before,
            hooks: harness.hooks.clone(),
            context,
            reports_session: reported && session.is_some_and(|session| !session.resume),
            reference_env: project.references.env.clone(),
        })
    }

    /// Whether `execute` must observe a native session ID for this run.
    pub fn reports_session_id(&self) -> bool {
        self.reports_session
    }

    /// Runs the hooks and the harness; `observe` receives a reported native session ID.
    /// The harness and its descendants are killed after `timeout`.
    pub fn execute(
        self,
        run_dir: &Path,
        mut task_input: impl Read,
        secrets: &[&EnvName],
        timeout: Duration,
        observe: impl Fn(String) + Sync,
    ) -> Result<ExitStatus> {
        for (name, content) in &self.files {
            fs::write(run_dir.join(name), content).with_context(|| format!("write {name}"))?;
        }
        let mut input = tempfile::tempfile_in(run_dir).context("create task spool")?;
        if let Some(template) = &self.stdin_template {
            let task = io::read_to_string(&mut task_input).context("read UTF-8 task")?;
            let prompt = Renderer::new(json!({ "task": task }))?
                .string(template)
                .context("render stdin template")?;
            input.write_all(prompt.as_bytes())?;
        } else {
            io::copy(&mut task_input, &mut input).context("spool task")?;
        }
        input.rewind()?;
        for (index, hook) in self.before.iter().enumerate() {
            self.hook(hook, secrets)
                .with_context(|| format!("hooks.before[{index}]"))?;
        }
        let mut command = self.command(&self.program, &self.args, secrets);
        // A process group lets the timeout reach tools the harness started.
        command.stdin(input).process_group(0);
        if self.hooks.stdout.is_some() {
            command.stdout(Stdio::piped());
        }
        if self.hooks.stderr.is_some() {
            command.stderr(Stdio::piped());
        }
        let mut child = command.spawn().context("run harness")?;
        let reported = OnceLock::new();
        let report = |id: String| {
            if self.reports_session && reported.set(()).is_ok() {
                observe(id);
            }
        };
        let streams = [
            (
                "stdout",
                child
                    .stdout
                    .take()
                    .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
                self.hooks.stdout.as_ref(),
            ),
            (
                "stderr",
                child
                    .stderr
                    .take()
                    .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
                self.hooks.stderr.as_ref(),
            ),
        ];
        let group = libc::pid_t::try_from(child.id()).context("harness process ID")?;
        let (finished, watch) = mpsc::channel::<()>();
        let timed_out = AtomicBool::new(false);
        let (relayed, status) = thread::scope(|scope| {
            let timed_out = &timed_out;
            scope.spawn(move || {
                if watch.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
                    timed_out.store(true, Ordering::Relaxed);
                    kill_group(group);
                }
            });
            let workers = streams
                .into_iter()
                .filter_map(|(stream, pipe, script)| Some((stream, pipe?, script?)))
                .map(|(stream, pipe, script)| {
                    let report = &report;
                    let context = &self.context;
                    scope.spawn(move || {
                        relay(stream, pipe, script, context, report)
                            .with_context(|| format!("hooks.{stream}"))
                    })
                })
                .collect::<Vec<_>>();
            let relayed = workers
                .into_iter()
                .try_for_each(|worker| worker.join().expect("output hook panicked"));
            if relayed.is_err() {
                kill_group(group);
            }
            let status = child.wait().context("wait for harness");
            drop(finished);
            (relayed, status)
        });
        let status = status?;
        if timed_out.load(Ordering::Relaxed) {
            eprintln!("agents: harness timed out after {}s", timeout.as_secs());
        }
        relayed?;
        if !self.hooks.after.is_empty() {
            let mut context = self.context.clone();
            context["status"] = json!(status.code());
            let renderer = Renderer::new(&context)?;
            for (index, hook) in self.hooks.after.iter().enumerate() {
                render_args(hook, &renderer)
                    .and_then(|hook| self.hook(&hook, secrets))
                    .with_context(|| format!("hooks.after[{index}]"))?;
            }
        }
        Ok(status)
    }

    /// Builds a command with the harness environment and without `agents` secrets.
    fn command(
        &self,
        program: impl AsRef<OsStr>,
        args: &[String],
        secrets: &[&EnvName],
    ) -> Command {
        let mut command = Command::new(program);
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
    }

    fn hook(&self, argv: &[String], secrets: &[&EnvName]) -> Result<()> {
        let Some((program, args)) = argv.split_first() else {
            return Ok(());
        };
        let status = self
            .command(program, args, secrets)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("run {program}"))?;
        ensure!(status.success(), "{program} failed ({status})");
        Ok(())
    }
}

/// Copies one output stream through `script`, line by line, in a scope that
/// keeps state for the whole run.
fn kill_group(group: libc::pid_t) {
    // SAFETY: kill(2) has no memory-safety preconditions.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
}

fn relay(
    stream: &str,
    pipe: impl Read,
    script: &Script,
    context: &Value,
    report: &impl Fn(String),
) -> Result<()> {
    let sandbox = Sandbox::new()?;
    let scope = sandbox.scope(context)?;
    let mut pipe = BufReader::new(pipe);
    let mut line = Vec::new();
    while pipe.read_until(b'\n', &mut line)? != 0 {
        let text = String::from_utf8_lossy(&line);
        let globals = json!({ "stream": stream, "line": text.trim_end_matches(['\n', '\r']) });
        let action: LineAction = serde_json::from_value(match scope.eval(script, &globals)? {
            Value::Null => json!({}),
            value => value,
        })
        .context("expected null or an object with `line` and `session`")?;
        if let Some(id) = action.session {
            ensure!(valid_native_id(&id), "invalid native session ID {id:?}");
            report(id);
        }
        let output = match action.line {
            None => Some(line.clone()),
            Some(None) => None,
            Some(Some(text)) => Some(format!("{text}\n").into_bytes()),
        };
        if let Some(output) = output {
            if stream == "stdout" {
                io::stdout().lock().write_all(&output)?;
            } else {
                io::stderr().lock().write_all(&output)?;
            }
        }
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
