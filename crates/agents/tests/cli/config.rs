use super::support::{Fixture, cli, copy_sample_harness_layer};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::symlink, path::Path, process::Stdio};

#[test]
fn complete_project_sample_renders_when_copied_to_the_active_name() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let sample = root.join("mail.agents.json");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/mail.agents.json"),
        &sample,
    )
    .unwrap();
    let render = || {
        cli(root)
            .args(["config", "render"])
            .arg(root)
            .output()
            .unwrap()
    };
    assert!(!render().status.success());
    fs::copy(sample, root.join(".agents.json")).unwrap();
    copy_sample_harness_layer(root);
    let output = render();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = cli(root).args(["mail", "addresses"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let addresses: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(addresses.len(), 2);
    assert!(
        addresses.iter().any(|entry| entry["agent"] == "reviewer"
            && entry["address"] == "agents+reviewer@example.test")
    );
    assert!(
        addresses
            .iter()
            .any(|entry| entry["agent"] == "builder" && entry["address"] == "builder@example.test")
    );
}

#[test]
fn project_follows_the_nearest_manifest_and_can_move_without_registration() {
    let fixture = Fixture::new();
    let parent = fixture.root.join("packages");
    let nested = parent.join("website");
    fs::create_dir_all(&nested).unwrap();
    let mut project = fixture.project.clone();
    project["agents"]["reviewer"] = json!({
        "harness": "review", "model": "nested", "description": "Nested",
        "instructions": "Nested project only."
    });
    fs::write(nested.join(".agents.json"), project.to_string()).unwrap();
    fs::create_dir(nested.join("src")).unwrap();
    let render = |directory: &Path| {
        let output = fixture
            .command()
            .args(["config", "render"])
            .arg(directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(
        render(&fixture.root)["cwd"],
        json!(fixture.root.canonicalize().unwrap())
    );
    assert_eq!(
        render(&nested.join("src"))["cwd"],
        json!(nested.canonicalize().unwrap())
    );
    let moved = parent.join("storefront");
    fs::rename(nested, &moved).unwrap();
    let plan = render(&moved);
    assert_eq!(plan["cwd"], json!(moved.canonicalize().unwrap()));
    assert!(plan["args"].as_array().unwrap().contains(&json!("nested")));
}

#[test]
fn project_instructions_fall_back_to_agents_md_without_combining_sources() {
    let mut fixture = Fixture::new();
    let render = |fixture: &Fixture| {
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
        serde_json::from_str::<Value>(plan["files"]["native.json"].as_str().unwrap()).unwrap()
    };
    assert_eq!(render(&fixture)["guide"], "Project guidance.\n");
    fixture.project["instructions"] = json!("Explicit project instructions.");
    fixture.save();
    let rendered = render(&fixture);
    assert_eq!(rendered["guide"], "Explicit project instructions.");
    assert_eq!(
        rendered["agent"]["instructions"],
        "Review {{ task }} literally.\n"
    );
    fixture.project["instructions"] = json!("");
    fixture.save();
    assert_eq!(render(&fixture)["guide"], "");
}

#[test]
fn discovers_project_skills_unless_agent_selects_its_own_list() {
    let mut fixture = Fixture::new();
    fixture.project["agents"]["reviewer"]
        .as_object_mut()
        .unwrap()
        .remove("skills");
    let skills = fixture.root.join(".agents/skills");
    fs::create_dir(skills.join("empty")).unwrap();
    fs::create_dir(skills.join("build")).unwrap();
    fs::write(skills.join("build/SKILL.md"), "# Build").unwrap();
    let render = |fixture: &Fixture| {
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
        serde_json::from_str::<Value>(plan["files"]["native.json"].as_str().unwrap()).unwrap()
    };
    fixture.save();
    let plan = render(&fixture);
    assert_eq!(plan["skills"].as_object().unwrap().len(), 2);
    assert!(
        plan["skills"]["build"]
            .as_str()
            .unwrap()
            .ends_with("/build/SKILL.md")
    );
    assert!(
        plan["skills"]["review"]
            .as_str()
            .unwrap()
            .ends_with("/review/SKILL.md")
    );

    fixture.project["skills"]["review"] = json!(".agents/skills/build/SKILL.md");
    fixture.save();
    let plan = render(&fixture);
    assert_eq!(plan["skills"]["review"], plan["skills"]["build"]);

    fixture.project["agents"]["reviewer"]["skills"] = json!([]);
    fixture.save();
    assert_eq!(render(&fixture)["skills"], json!({}));
}

#[test]
fn project_can_be_a_single_file_or_ordered_fragments() {
    let fixture = Fixture::new();
    let root = fixture.root.join("subproject");
    let fragments = root.join(".agents.d");
    fs::create_dir_all(&fragments).unwrap();
    fs::write(root.join("guidance.md"), "Shared guidance.").unwrap();
    let base = root.join(".agents.json");
    let manifest = json!({
        "version": 1, "agent": "reviewer", "instructions": "Shared guidance.",
        "harnesses": fixture.project["harnesses"],
        "mcp": fixture.project["mcp"],
        "agents": {"reviewer": {
            "harness": "review",
            "model": "base",
            "description": "One-file agent",
            "instructions": "Base instructions",
            "mcp": ["search"]
        }}
    });
    fs::write(&base, manifest.to_string()).unwrap();
    let render = || {
        let output = fixture
            .command()
            .args(["config", "render"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        let native: Value =
            serde_json::from_str(plan["files"]["native.json"].as_str().unwrap()).unwrap();
        (plan, native)
    };
    let (_, native) = render();
    assert_eq!(native["agent"]["instructions"], "Base instructions");
    assert_eq!(native["guide"], "Shared guidance.");
    assert_eq!(native["mcp"]["search"]["type"], "stdio");

    // Write out of order; the files' names, not insertion order, decide priority.
    fs::write(
        fragments.join("90-local.json"),
        r#"{"agents":{"reviewer":{"model":null,"instructions":"Final instructions","mcp":[]}}}"#,
    )
    .unwrap();
    fs::write(
        fragments.join("10-team.json"),
        r#"{"agents":{"reviewer":{"model":"team","instructions":"Team instructions"}}}"#,
    )
    .unwrap();
    fs::write(fragments.join("README.md"), "Ignored").unwrap();
    let (plan, native) = render();
    assert!(!plan["args"].as_array().unwrap().contains(&json!("--model")));
    assert_eq!(native["agent"]["description"], "One-file agent");
    assert_eq!(native["agent"]["instructions"], "Final instructions");
    assert_eq!(native["mcp"], json!({}));

    // No single fragment needs to be a complete configuration.
    fs::rename(&base, fragments.join("00-base.json")).unwrap();
    let (_, native) = render();
    assert_eq!(native["agent"]["instructions"], "Final instructions");
}

#[test]
fn project_fragment_cannot_escape_its_directory() {
    let fixture = Fixture::new();
    let outside = fixture._dir.path().join("elsewhere.json");
    fs::write(
        &outside,
        r#"{"agents":{"reviewer":{"model":"unexpected"}}}"#,
    )
    .unwrap();
    let fragments = fixture.root.join(".agents.d");
    fs::create_dir(&fragments).unwrap();
    symlink(&outside, fragments.join("10-outside.json")).unwrap();
    let output = fixture
        .command()
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("escapes"));
}

#[test]
fn json5_jsonc_and_yaml_share_the_schema_and_layer_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let layers = root.join(".agents.d");
    copy_sample_harness_layer(root);
    let yaml = r#"
# yaml-language-server: $schema=./agents.schema.json
version: 1
agent: reviewer
agents:
  reviewer:
    harness: claude
    model: base
    description: Review
    instructions: Review the project.
"#;
    let yaml_path = root.join(".agents.yaml");
    fs::write(&yaml_path, yaml).unwrap();
    fs::write(
        layers.join("10-early.jsonc"),
        r#"{
            // JSONC accepts comments and trailing commas.
            "agents": {"reviewer": {"model": "early",},},
        }"#,
    )
    .unwrap();
    fs::write(
        layers.join("15-json5.json5"),
        "{ agents: { reviewer: { instructions: 'Written in a JSON5 layer.' } } }",
    )
    .unwrap();
    fs::write(
        layers.join("20-late.yml"),
        "agents:\n  reviewer:\n    model: late\n",
    )
    .unwrap();
    let render = || cli(root).args(["config", "render"]).output().unwrap();
    let output = render();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(plan["args"].as_array().unwrap().contains(&json!("late")));
    assert!(
        plan["files"]["instructions.md"]
            .as_str()
            .unwrap()
            .contains("Written in a JSON5 layer.")
    );

    let json5_path = root.join(".agents.json5");
    fs::write(
        &json5_path,
        r#"{
        /* Base file in JSON5. */
        version: 0x1, agent: 'reviewer',
        agents: { reviewer: { harness: 'claude', description: 'Review', instructions: 'Review.' } },
    }"#,
    )
    .unwrap();
    let output = render();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("multiple .agents main files"));
    fs::remove_file(yaml_path).unwrap();
    assert!(render().status.success());
    let jsonc_path = root.join(".agents.jsonc");
    fs::rename(json5_path, &jsonc_path).unwrap();
    let output = render();
    assert!(
        !output.status.success(),
        "JSONC must reject JSON5-only syntax"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("parse JSONC object"));
    fs::write(
        &jsonc_path,
        r#"{
            // JSONC permits comments and trailing commas, but keys must be quoted.
            "version": 1, "agent": "reviewer",
            "agents": {"reviewer": {"harness": "claude", "description": "Review", "instructions": "Review.",},},
        }"#,
    )
    .unwrap();
    assert!(render().status.success());

    let nonfinite = layers.join("30-nonfinite.json5");
    fs::write(&nonfinite, "{ agents: { reviewer: { model: NaN } } }").unwrap();
    let output = render();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-finite numbers"));
    fs::remove_file(nonfinite).unwrap();

    fs::rename(jsonc_path, root.join(".agents.json")).unwrap();
    assert!(!render().status.success(), "plain JSON must remain strict");
}

#[test]
fn external_references_merge_before_resolution_and_are_hidden_in_preview() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let layers = root.join(".agents.d");
    fs::create_dir(&layers).unwrap();
    fs::write(root.join("secret.txt"), "file-value\n").unwrap();
    fs::write(
        root.join(".agents.json"),
        json!({
            "version": 1,
            "agent": "test",
            "agents": {"test": {
                "harness": "fake", "description": "Test", "instructions": "Check values",
                "mcp": ["tool"]
            }},
            "mcp": {"tool": {"type": "stdio", "command": "example", "env": {
                "TOKEN": {"env": "AGENTS_TEST_REFERENCE"},
                "FILE": {"env": "AGENTS_TEST_SUPERSEDED"}
            }}},
            "harnesses": {"fake": {
                "program": "/bin/sh", "capabilities": {"mcp": ["stdio"]},
                "args": ["-c", r#"test "${AGENTS_TEST_REFERENCE-unset}" = unset &&
                    test "$1" = 'env-value' && test "$2" = "$(printf 'file-value\n.')" && printf ok"#,
                    "test", "{{ mcp.tool.env.TOKEN }}", "{{ mcp.tool.env.FILE }}."]
            }}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        layers.join("10-secret.yaml"),
        "mcp:\n  tool:\n    env:\n      FILE: { file: secret.txt }\n",
    )
    .unwrap();
    let command = || {
        let mut cmd = cli(root);
        cmd.env_remove("AGENTS_TEST_SUPERSEDED")
            .env("AGENTS_TEST_REFERENCE", "env-value");
        cmd
    };
    let output = command().args(["config", "render"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("env-value") && !stdout.contains("file-value"));
    let preview: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(preview["redacted"], true);

    let output = command().arg("run").stdin(Stdio::null()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok");

    let output = command()
        .env_remove("AGENTS_TEST_REFERENCE")
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("AGENTS_TEST_REFERENCE"));
}

#[test]
fn shared_schema_matches_the_cli_and_resolves_its_references() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir().unwrap();
    let output = cli(directory.path())
        .args(["config", "schema"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = fs::read(root.join("../../agents.schema.json")).unwrap();
    assert_eq!(output.stdout, expected, "regenerate the shared schema");

    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(schema["allOf"][0]["$ref"], "#/$defs/ProjectFragment");
    for entry in ["Harness", "SendMail", "ProjectFragment"] {
        assert!(
            schema["$defs"][entry].is_object(),
            "missing schema entry {entry}"
        );
    }
    fn check_references(schema: &Value, node: &Value) {
        match node {
            Value::Object(object) => {
                if let Some(reference) = object
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|s| s.strip_prefix('#'))
                {
                    assert!(
                        schema.pointer(reference).is_some(),
                        "unresolved schema reference {reference}"
                    );
                }
                for value in object.values() {
                    check_references(schema, value);
                }
            }
            Value::Array(array) => {
                for value in array {
                    check_references(schema, value);
                }
            }
            _ => {}
        }
    }
    check_references(&schema, &schema);
}

#[test]
fn malformed_configuration_fails_during_decode() {
    let mut fixture = Fixture::new();
    fixture.project["version"] = json!(2);
    fixture.save();
    let output = fixture
        .command()
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported version 2"));

    fixture.project["version"] = json!(1);
    fixture.project["agent"] = json!("../outside");
    fixture.save();
    let output = fixture
        .command()
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected an ID"));
}

#[test]
fn rejects_unknown_mcp_and_escaped_agent_files() {
    let mut fixture = Fixture::new();
    fixture.project["agents"]["reviewer"]["mcp"] = json!(["unknown"]);
    fixture.save();
    let output = fixture
        .command()
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown MCP"));

    fixture.project["agents"]["reviewer"]["mcp"] = json!(["search"]);
    fixture.project["agents"]["reviewer"]["instructions"] =
        json!({"file": ".agents/agents/reviewer/instructions.md"});
    fixture.save();

    let agent = fixture.root.join(".agents/agents/reviewer/instructions.md");
    fs::remove_file(&agent).unwrap();
    let outside = fixture._dir.path().join("other.md");
    fs::write(&outside, "outside").unwrap();
    symlink(outside, agent).unwrap();
    let output = fixture
        .command()
        .args(["config", "check"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("escapes"));
}

#[test]
fn named_defaults_and_mcp_patterns_respect_explicit_overrides() {
    let mut fixture = Fixture::new();
    let reviewer = fixture.project["agents"]["reviewer"].take();
    fixture.project["agents"]["default"] = reviewer;
    fixture.project["agents"]["default"]["mail"]["address"] = json!("reviewer@example.test");
    fixture.project["agents"]["default"]["mcp"] = json!(["*", "!dev-*", "dev-keep", "!search"]);
    fixture.project["agents"]["reviewer"] = json!({"model": null, "description": "Override"});
    fixture.project["harnesses"]["default"] = fixture.project["harnesses"]["review"].take();
    fixture.project["harnesses"]["review"] = json!({});
    fixture.project["mcp"] = json!({
        "default": {"type": "stdio", "command": "example"},
        "search": {},
        "docs": {},
        "dev-keep": {"disable": false},
        "dev-skip": {},
        "disabled": {"env": {"TOKEN": {"env": "AGENTS_TEST_DISABLED_SECRET"}}}
    });
    fixture.save();
    let layers = fixture.root.join(".agents.d");
    fs::create_dir(&layers).unwrap();
    fs::write(
        layers.join("90-local.yaml"),
        "mcp:\n  disabled:\n    disable: true\n",
    )
    .unwrap();
    let output = fixture
        .command()
        .env_remove("AGENTS_TEST_DISABLED_SECRET")
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!plan["args"].as_array().unwrap().contains(&json!("--model")));
    let rendered: Value =
        serde_json::from_str(plan["files"]["native.json"].as_str().unwrap()).unwrap();
    assert_eq!(rendered["agent"]["description"], "Override");
    assert_eq!(
        rendered["mcp"],
        json!({
            "docs": {"type": "stdio", "command": "example", "args": [], "env": {}},
            "dev-keep": {"type": "stdio", "command": "example", "args": [], "env": {}}
        })
    );
    let output = fixture
        .command()
        .args(["mail", "addresses"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let addresses: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(addresses.len(), 1);
    assert_eq!(addresses[0]["agent"], "reviewer");
    assert_eq!(addresses[0]["address"], "reviewer@example.test");

    fixture.project["agents"]["reviewer"]["mcp"] = json!([]);
    fixture.save();
    let output = fixture
        .command()
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rendered: Value =
        serde_json::from_str(plan["files"]["native.json"].as_str().unwrap()).unwrap();
    assert_eq!(rendered["mcp"], json!({}));
}

#[test]
fn harnesses_require_project_definitions_and_merge_with_later_layers() {
    let mut fixture = Fixture::new();
    fixture.project["agents"]["reviewer"]["harness"] = json!("claude");
    fixture.project["agents"]["reviewer"]["mcp"] = json!([]);
    fixture.project["agents"]["reviewer"]["skills"] = json!([]);
    fixture.save();

    let output = fixture
        .command()
        .args(["config", "render"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unregistered harness claude"));

    let render = || {
        fixture
            .command()
            .args(["config", "render"])
            .output()
            .unwrap()
    };
    let layers = fixture.root.join(".agents.d");
    fs::create_dir(&layers).unwrap();
    fs::write(
        layers.join("10-harness.json"),
        json!({
            "harnesses": {"claude": {
                "program": "custom-claude",
                "args": ["base"],
                "stdin": "Task: {{ task }}"
            }}
        })
        .to_string(),
    )
    .unwrap();
    for text in ["first {{ model }}", "second {{ model }}"] {
        fs::write(
            layers.join("20-override.json"),
            json!({
                "harnesses": {"claude": {
                    "args": [text]
                }}
            })
            .to_string(),
        )
        .unwrap();
        let output = render();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(plan["program"], "custom-claude");
        assert_eq!(
            plan["args"],
            json!([text.replace("{{ model }}", "test model")])
        );
        assert_eq!(plan["stdin_template"], "Task: {{ task }}");
    }
}
