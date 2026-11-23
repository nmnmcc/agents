use super::support::{Fixture, cli, copy_sample_harness_layer, sample_project};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[test]
fn previews_and_runs_from_nested_directory() {
    let fixture = Fixture::new();
    let settings = fixture._dir.path().join("home/.claude/settings.json");
    let preview = fixture
        .command()
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["args"][0], "<run-dir>/native.json");
    assert_eq!(preview["args"][1], "one ; $argument");
    assert_eq!(preview["child_env"]["AGENTS_TEST_INJECT"], "test model");
    assert_eq!(preview["child_env"]["AGENTS_TEST_REMOVE"], Value::Null);
    assert!(!fixture.root.join("native.json").exists());

    let output = fixture.handle_mail("reviewer", "--not-a-flag $(no) {{ model }}");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("agents: reviewer: harness exit status: 23"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    let (native, stdin) = output.split_once('\n').unwrap();
    let native: Value = serde_json::from_str(native).unwrap();
    assert_eq!(
        native["agent"]["instructions"],
        "Review {{ task }} literally.\n"
    );
    assert_eq!(native["guide"], "Project guidance.\n");
    assert_eq!(native["mcp"]["search"]["args"][0], "a b");
    assert!(!Path::new(native["path"].as_str().unwrap()).exists());
    assert_eq!(
        fs::read_to_string(settings).unwrap(),
        r#"{"user":"original"}"#
    );
    assert!(
        native["skills"]["review"]
            .as_str()
            .unwrap()
            .ends_with("/review/SKILL.md")
    );
    assert!(stdin.starts_with("Task:\nThe new message in your inbox follows."));
    assert!(stdin.contains("--not-a-flag $(no) {{ model }}"), "{stdin}");
}

#[test]
fn selected_agent_chooses_its_own_harness_and_model() {
    let mut fixture = Fixture::new();
    fixture.project["harnesses"]["build"] = json!({
        "program": "/bin/sh",
        "args": ["-c", "printf '%s' \"$1\"", "build", "{{ model }}"],
        "capabilities": {"mcp": ["stdio"]}
    });
    fixture.project["agents"]["builder"] = json!({
        "harness": "build",
        "skills": [],
        "model": "builder model",
        "description": "Build",
        "instructions": "Build it."
    });
    fixture.save();
    let default = fixture
        .command()
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(default.status.success());
    let plan: Value = serde_json::from_slice(&default.stdout).unwrap();
    assert!(
        plan["args"]
            .as_array()
            .unwrap()
            .contains(&json!("test model"))
    );

    let selected = fixture.handle_mail("builder", "Build it.");
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    assert_eq!(selected.stdout, b"builder model");

    for field in ["harness", "model"] {
        fixture.project[field] = json!("misplaced");
        fixture.save();
        let output = fixture
            .command()
            .args(["config", "check"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("unknown field `{field}`"))
        );
        fixture.project.as_object_mut().unwrap().remove(field);
    }
}

#[test]
fn script_arguments_keep_their_position_and_expand_to_any_count() {
    let mut fixture = Fixture::new();
    let mut config = fixture.project["harnesses"]["review"].clone();
    config["args"] = json!([
        "before",
        {"js": "model === null ? null : ['--model', model, ...('search' in mcp ? ['--search'] : [])]"},
        "after"
    ]);
    let render = |fixture: &mut Fixture, config: &Value| {
        fixture.project["harnesses"]["review"] = config.clone();
        fixture.save();
        fixture
            .command()
            .args(["config", "render"])
            .output()
            .unwrap()
    };
    let output = render(&mut fixture, &config);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        plan["args"],
        json!(["before", "--model", "test model", "--search", "after"])
    );

    fixture.project["agents"]["reviewer"]["model"] = Value::Null;
    let output = render(&mut fixture, &config);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["args"], json!(["before", "after"]));

    for script in ["42", "[1]"] {
        config["args"][1] = json!({ "js": script });
        let output = render(&mut fixture, &config);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("expected a"));
    }
}

#[test]
fn javascript_renders_arguments_environment_and_files() {
    let mut fixture = Fixture::new();
    let mut config = fixture.project["harnesses"]["review"].clone();
    config["args"] = json!([
        {"js": "['--model', model]"},
        {"js": "null"},
        {"js": "agent.id.toUpperCase()"}
    ]);
    config["env"] = json!({"AGENTS_TEST_INJECT": {"js": "`${agent.id}:${model}`"}, "AGENTS_TEST_REMOVE": {"js": "null"}});
    config["files"]["native.json"]["template"] =
        json!({"js": "({agent: agent.id, mcp: Object.keys(mcp)})"});
    fixture.project["harnesses"]["review"] = config;
    fixture.save();
    let output = fixture
        .command()
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["args"], json!(["--model", "test model", "REVIEWER"]));
    assert_eq!(
        plan["child_env"]["AGENTS_TEST_INJECT"],
        "reviewer:test model"
    );
    assert_eq!(plan["child_env"]["AGENTS_TEST_REMOVE"], Value::Null);
    let native: Value =
        serde_json::from_str(plan["files"]["native.json"].as_str().unwrap()).unwrap();
    assert_eq!(native, json!({"agent": "reviewer", "mcp": ["search"]}));
}

#[test]
fn sample_project_renders_without_installed_harness() {
    let project = sample_project();
    let output = cli(&project).args(["config", "render"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["program"], "claude");
    let instructions = plan["files"]["instructions.md"].as_str().unwrap();
    assert!(instructions.contains("Review the requested change"));
    assert!(instructions.contains("/.agents/skills/review/SKILL.md"));
    let mcp: Value = serde_json::from_str(plan["files"]["mcp.json"].as_str().unwrap()).unwrap();
    assert_eq!(mcp, json!({"mcpServers": {}}));
}

#[test]
fn adapters_translate_the_same_agent_and_both_mcp_transports() {
    let mut fixture = Fixture::new();
    copy_sample_harness_layer(&fixture.root);
    fixture.project["agents"]["reviewer"]["mcp"] = json!(["search", "remote"]);
    fixture.project["mcp"]["remote"] = json!({
        "type": "streamable-http", "url": "https://example.test/mcp",
        "headers": {"X-Example": "test-header-value"}
    });
    fixture.project["mcp"]["search"]
        .as_object_mut()
        .unwrap()
        .remove("type");
    fixture.project["mcp"]["search"]["env"] = json!({"ODD_VALUE": "a \"quote\"\na newline"});

    let mut plans = Vec::new();
    for harness in ["claude", "codex", "opencode"] {
        if harness != "claude" {
            fixture.project["mcp"]["search"]["cwd"] = json!("tools");
        }
        fixture.project["agents"]["reviewer"]["harness"] = json!(harness);
        fixture.save();
        let output = fixture
            .command()
            .args(["config", "render"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["program"], harness);
        assert!(
            plan["args"]
                .as_array()
                .unwrap()
                .windows(2)
                .any(|args| args == ["--model", "test model"])
        );
        plans.push(plan);
    }

    let claude: Value =
        serde_json::from_str(plans[0]["files"]["mcp.json"].as_str().unwrap()).unwrap();
    assert_eq!(claude["mcpServers"]["search"]["type"], "stdio");
    assert_eq!(claude["mcpServers"]["remote"]["type"], "http");
    assert_eq!(
        claude["mcpServers"]["remote"]["headers"]["X-Example"],
        "test-header-value"
    );

    let codex = plans[1]["args"].as_array().unwrap();
    let mcp = codex
        .iter()
        .find_map(|arg| arg.as_str().filter(|arg| arg.starts_with("mcp_servers=")))
        .unwrap();
    assert!(!mcp.contains("test-header-value"));
    let mcp: toml::Table = toml::from_str(mcp).unwrap();
    assert_eq!(
        mcp["mcp_servers"]["search"]["env"]["ODD_VALUE"].as_str(),
        Some("a \"quote\"\na newline")
    );
    assert_eq!(
        mcp["mcp_servers"]["remote"]["required"].as_bool(),
        Some(true)
    );
    assert_eq!(mcp["mcp_servers"]["search"]["cwd"].as_str(), Some("tools"));
    let header_env = mcp["mcp_servers"]["remote"]["env_http_headers"]["X-Example"]
        .as_str()
        .unwrap();
    assert_eq!(plans[1]["child_env"][header_env], "test-header-value");
    let instructions = codex
        .iter()
        .find_map(|arg| {
            arg.as_str()
                .filter(|arg| arg.starts_with("developer_instructions="))
        })
        .unwrap();
    let instructions: toml::Table = toml::from_str(instructions).unwrap();
    let instructions = instructions["developer_instructions"].as_str().unwrap();
    assert!(instructions.contains("Review {{ task }} literally."));
    assert!(instructions.contains("/review/SKILL.md"));
    assert!(instructions.contains("Project guidance."));

    let opencode: Value =
        serde_json::from_str(plans[2]["files"]["opencode.json"].as_str().unwrap()).unwrap();
    assert_eq!(
        plans[2]["child_env"]["OPENCODE_CONFIG"],
        "<run-dir>/opencode.json"
    );
    assert_eq!(opencode["agents"]["agents-reviewer"]["mode"], "primary");
    assert!(
        opencode["agents"]["agents-reviewer"]["system"]
            .as_str()
            .unwrap()
            .contains("/review/SKILL.md")
    );
    assert_eq!(
        opencode["mcp"]["servers"]["search"]["command"],
        json!(["example", "a b"])
    );
    assert_eq!(opencode["mcp"]["servers"]["search"]["cwd"], "tools");
    assert_eq!(opencode["mcp"]["servers"]["remote"]["type"], "remote");
    assert_eq!(
        opencode["mcp"]["servers"]["remote"]["headers"]["X-Example"],
        "test-header-value"
    );
}

#[test]
fn adapters_handle_legacy_and_client_specific_transports_explicitly() {
    let mut fixture = Fixture::new();
    copy_sample_harness_layer(&fixture.root);
    fixture.project["agents"]["reviewer"]["mcp"] = json!(["remote"]);
    for (transport, url) in [
        ("sse", "https://example.test/sse"),
        ("ws", "wss://example.test/mcp"),
    ] {
        // A header named "file" is a literal header, not a file reference.
        fixture.project["mcp"]["remote"] =
            json!({"type": transport, "url": url, "headers": {"file": "literal"}});
        for harness in ["claude", "codex", "opencode"] {
            fixture.project["agents"]["reviewer"]["harness"] = json!(harness);
            fixture.save();
            let output = fixture
                .command()
                .args(["config", "render"])
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if harness == "claude" {
                assert!(output.status.success(), "{stderr}");
                let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
                let file: Value =
                    serde_json::from_str(plan["files"]["mcp.json"].as_str().unwrap()).unwrap();
                assert_eq!(
                    file["mcpServers"]["remote"],
                    fixture.project["mcp"]["remote"]
                );
            } else {
                assert!(!output.status.success());
                assert!(
                    stderr.contains("does not support the transport for MCP"),
                    "{stderr}"
                );
            }
        }
    }
}

#[test]
fn remote_header_references_are_redacted_in_previews() {
    let mut fixture = Fixture::new();
    copy_sample_harness_layer(&fixture.root);
    fixture.project["agents"]["reviewer"]["harness"] = json!("codex");
    fixture.project["agents"]["reviewer"]["mcp"] = json!(["remote"]);
    fixture.project["mcp"]["remote"] = json!({
        "type": "http", "url": "https://example.test/mcp",
        "headers": {"Authorization": {"env": "AGENTS_TEST_MCP_HEADER"}}
    });
    fixture.save();
    let output = fixture
        .command()
        .env("AGENTS_TEST_MCP_HEADER", "Bearer dummy-secret")
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let preview: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(preview["redacted"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("dummy-secret"));
}

#[test]
fn hooks_surround_the_run_and_filter_its_output() {
    let mut fixture = Fixture::new();
    let program = fixture.root.join("hooked");
    fs::write(
        &program,
        "#!/bin/sh\nprintf 'one\\nskip\\ntwo\\n'\nprintf 'warn\\n' >&2\nexit 3\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.project["harnesses"]["review"] = json!({
        "program": program,
        "args": [],
        "capabilities": {"mcp": ["stdio"], "skills": true},
        "hooks": {
            "before": [["sh", "-c", "echo \"before $AGENTS_HOOK\" >> hooks.log"]],
            "after": [[{"js": "['sh', '-c', `echo after ${status} >> hooks.log`]"}]],
            "stdout": {"js": "globalThis.n = (globalThis.n ?? 0) + 1; \
                line === 'skip' ? {line: null} : {line: `${n}:${line}`}"},
            "stderr": {"js": "({line: `${stream}:${line}`})"}
        },
        "env": {"AGENTS_HOOK": "{{ agent.id }}"}
    });
    fixture.save();
    let run = |fixture: &Fixture| fixture.handle_mail("reviewer", "Check.");
    let output = run(&fixture);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("harness exit status: 3"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1:one\n3:two\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("stderr:warn\n"));
    let log = fixture.root.join("hooks.log");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "before reviewer\nafter 3\n"
    );

    fs::remove_file(&log).unwrap();
    fixture.project["harnesses"]["review"]["hooks"]["before"] = json!([[], ["false"]]);
    fixture.save();
    let output = run(&fixture);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hooks.before[1]"));
    assert!(output.stdout.is_empty() && !log.exists());

    fixture.project["harnesses"]["review"]["hooks"] = json!({"stdout": {"js": "'bad'"}});
    fixture.project["harnesses"]["review"]["capabilities"]["sessions"] = json!("reported");
    fixture.save();
    let output = run(&fixture);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hooks.stdout"));
}

#[test]
fn mail_runs_once_per_message_until_retried_and_time_out() {
    let mut fixture = Fixture::new();
    let program = fixture.root.join("counted");
    let runs = fixture.root.join("runs.log");
    let hang = fixture.root.join("hang");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\necho run >> '{}'\nif test -e '{}'; then sh -c 'sleep 30' & wait; fi\n",
            runs.display(),
            hang.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.project["harnesses"]["review"] = json!({
        "program": program,
        "args": [],
        "capabilities": {"mcp": ["stdio"], "skills": true}
    });
    fixture.project["agents"]["reviewer"]["timeout"] = json!(1);
    fixture.save();
    let count = || fs::read_to_string(&runs).unwrap().lines().count();
    let once = || fixture.command().arg("--once").output().unwrap();
    let list = || -> Vec<Value> {
        let output = fixture
            .command()
            .args(["mail", "list", "--agent", "reviewer"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let page: Value = serde_json::from_slice(&output.stdout).unwrap();
        page["messages"].as_array().unwrap().clone()
    };

    let output = fixture.handle_mail("reviewer", "One.");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(count(), 1);
    // The agent left the message unread, but a run already started for it.
    assert!(once().status.success());
    assert_eq!(count(), 1);
    let message = &list()[0];
    assert_eq!(
        (
            &message["unread"],
            &message["dispatched"],
            &message["failed"]
        ),
        (&json!(true), &json!(true), &json!(false))
    );

    let mail = &message["mail"];
    let retry = fixture
        .command()
        .args(["mail", "retry", "--agent", "reviewer"])
        .arg(mail["uid"].to_string())
        .args(["--uid-validity", &mail["uid_validity"].to_string()])
        .output()
        .unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert!(once().status.success());
    assert_eq!(count(), 2);

    fs::write(&hang, "").unwrap();
    let started = std::time::Instant::now();
    let output = fixture.handle_mail("reviewer", "Two.");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("harness timed out after 1s"));
    let messages = list();
    assert_eq!(messages[0]["failed"], true);
    assert_eq!(messages[1]["failed"], false);
    assert_eq!(count(), 3);
}
