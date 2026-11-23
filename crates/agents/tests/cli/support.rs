use hopf_mailbox::{AppendGuard, IndexConfig, MboxMailbox};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub(super) fn cli(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agents"));
    command.current_dir(directory);
    command
}

/// The mail settings `handle_mail` expects: local addresses and a trusted human sender.
pub(super) fn local_mail() -> Value {
    json!({"address": "{{ agent.id }}@example.test", "trust": ["human@example.test"]})
}

/// Delivers `body` from the trusted human to `agent`, then makes one watcher pass.
pub(super) fn handle_mail(command: impl Fn() -> Command, agent: &str, body: &str) -> Output {
    let cwd = command();
    let root = cwd
        .get_current_dir()
        .unwrap()
        .ancestors()
        .find(|path| path.join(".agents.json").is_file())
        .unwrap();
    local_message(
        root,
        &format!("{agent}@example.test"),
        "human@example.test",
        &format!("From: human@example.test\r\nSubject: Task\r\n\r\n{body}\r\n"),
    );
    command()
        .arg("--once")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Seed the private post-verification store, without using external crypto in CLI tests.
pub(super) fn local_message(root: &Path, to: &str, from: &str, raw: &str) {
    let directory = root.join(".agents/mail");
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("inbox.mbox");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    let mut mailbox =
        MboxMailbox::open(&path, "inbox.mbox", false, IndexConfig::default()).unwrap();
    let mut append = AppendGuard::start(&mut mailbox, &BTreeSet::new(), None).unwrap();
    append
        .append_content(
            format!("X-Agents-Local-To: {to}\r\nX-Agents-Local-From: {from}\r\n{raw}").as_bytes(),
        )
        .unwrap();
    append.commit().unwrap();
}

pub(super) fn sample_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/project")
}

pub(super) fn copy_sample_harness_layer(root: &Path) {
    let layer = ".agents.d/00-harnesses.yaml";
    fs::create_dir_all(root.join(".agents.d")).unwrap();
    fs::copy(sample_project().join(layer), root.join(layer)).unwrap();
}

pub(super) struct Fixture {
    pub(super) _dir: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) project: Value,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project with spaces");
        let agent = root.join(".agents/agents/reviewer");
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            agent.join("instructions.md"),
            "Review {{ task }} literally.\n",
        )
        .unwrap();
        fs::write(root.join("AGENTS.md"), "Project guidance.\n").unwrap();
        let settings = dir.path().join("home/.claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(settings, r#"{"user":"original"}"#).unwrap();
        let skills = root.join(".agents/skills/review");
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("SKILL.md"), "# Review skill").unwrap();
        let program = dir.path().join("fake harness");
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
test -f AGENTS.md
test "$#" -eq 4
test "$2" = 'one ; $argument'
test "$3" = '--model'
test "$4" = 'test model'
test "$AGENTS_TEST_INJECT" = 'test model'
test "${AGENTS_TEST_REMOVE-unset}" = 'unset'
test "$(cat "$HOME/.claude/settings.json")" = '{"user":"original"}'
cat "$1"
cat
exit 23
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let harness = json!({
                "program": program,
                "args": ["{{ paths['native.json'] }}", "one ; $argument",
                         {"js": "model === null ? null : ['--model', model]"}],
                "env": {"AGENTS_TEST_INJECT": "{{ model }}", "AGENTS_TEST_REMOVE": null},
                "capabilities": {"mcp": ["stdio"], "skills": true},
                "files": {"native.json": {"template": "{{ {'agent': agent, 'guide': project_instructions, 'skills': skills, 'mcp': mcp, 'path': paths['native.json']} | tojson }}\n", "format": "json"}},
                "stdin": "Task:\n{{ task }}"
        });
        let mut fixture = Self {
            project: json!({
                "version": 1,
                "agent": "reviewer",
                "agents": {"reviewer": {
                    "harness": "review", "model": "test model", "description": "Review",
                    "instructions": "Review {{ task }} literally.\n",
                    "mcp": ["search"], "skills": ["review"]
                }},
                "harnesses": {"review": harness},
                "mcp": {"search":{"type":"stdio","command":"example","args":["a b"]}}
            }),
            root,
            _dir: dir,
        };
        fixture.project["mail"] = local_mail();
        fixture.save();
        fixture
    }

    /// Delivers `body` to `agent` and makes one watcher pass.
    pub(super) fn handle_mail(&self, agent: &str, body: &str) -> Output {
        handle_mail(|| self.command(), agent, body)
    }

    pub(super) fn save(&mut self) {
        fs::write(self.root.join(".agents.json"), self.project.to_string()).unwrap();
    }

    pub(super) fn command(&self) -> Command {
        let mut command = cli(&self.root);
        command
            .current_dir(self.root.join(".agents/agents"))
            .env("HOME", self._dir.path().join("home"))
            .env("AGENTS_TEST_INJECT", "original")
            .env("AGENTS_TEST_REMOVE", "original");
        command
    }
}
