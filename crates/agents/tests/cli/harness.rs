use super::support::{Fixture, cli, copy_sample_harness_layer, sample_project};
use serde_json::{Value, json};
use std::{fs, io::Write, path::Path, process::Stdio};

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

    let mut child = fixture
        .command()
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"--not-a-flag $(no) {{ model }}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(23),
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
    assert_eq!(stdin, "Task:\n--not-a-flag $(no) {{ model }}\n");
}

#[test]
fn selected_agent_chooses_its_own_harness_and_model() {
    let mut fixture = Fixture::new();
    fixture.project["harnesses"]["build"] = json!({
        "version": 1, "program": "/bin/sh",
        "args": ["-c", "printf '%s' \"$1\"", "build", "{{ model }}"]
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

    let selected = fixture
        .command()
        .args(["run", "--agent", "builder"])
        .output()
        .unwrap();
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
fn conditional_arguments_use_their_position_and_skip_inactive_groups() {
    let mut fixture = Fixture::new();
    let mut config = fixture.project["harnesses"]["review"].clone();
    config["args"] = json!([
        "before",
        {"when": "model is not none", "args": [
            "--model", "{{ model }}",
            {"when": "mcp.search is defined", "args": ["--search"]}
        ]},
        {"when": "false", "args": [{"template": "not-loaded.j2"}]},
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

    config["args"][1]["when"] = json!("42");
    let output = render(&mut fixture, &config);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("`when` must evaluate to a boolean"));
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
    fixture.project["mcp"]["remote"] = json!({"type":"http","url":"https://example.test/mcp"});
    fixture.project["mcp"]["search"]["env"] = json!({"ODD_VALUE": "a \"quote\"\na newline"});

    let mut plans = Vec::new();
    for harness in ["claude", "codex", "opencode"] {
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

    let codex = plans[1]["args"].as_array().unwrap();
    let mcp = codex
        .iter()
        .find_map(|arg| arg.as_str().filter(|arg| arg.starts_with("mcp_servers=")))
        .unwrap();
    let mcp: toml::Table = toml::from_str(mcp).unwrap();
    assert_eq!(
        mcp["mcp_servers"]["search"]["env"]["ODD_VALUE"].as_str(),
        Some("a \"quote\"\na newline")
    );
    assert_eq!(
        mcp["mcp_servers"]["remote"]["required"].as_bool(),
        Some(true)
    );
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
    assert_eq!(plans[2]["child_env"]["OPENCODE_DB"], ":memory:");
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
    assert_eq!(opencode["mcp"]["servers"]["remote"]["type"], "remote");
}
