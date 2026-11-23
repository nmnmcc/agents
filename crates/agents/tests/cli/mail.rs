use super::support::{Fixture, copy_sample_harness_layer, local_message};
use serde_json::{Value, json};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, process::Stdio};

#[test]
fn watcher_resumes_claude_mail_sessions() {
    watcher_resumes_mail_sessions("claude");
}

#[test]
fn watcher_resumes_codex_mail_sessions() {
    watcher_resumes_mail_sessions("codex");
}

#[test]
fn watcher_resumes_opencode_mail_sessions() {
    watcher_resumes_mail_sessions("opencode");
}

fn watcher_resumes_mail_sessions(harness: &str) {
    use std::{
        process::Child,
        thread,
        time::{Duration, Instant},
    };

    struct Service(Child);
    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let mut fixture = Fixture::new();
    fixture.project["agents"]["builder"] = fixture.project["agents"]["reviewer"].clone();
    fixture.project["agents"]["builder"]["description"] = json!("Build");
    fixture.project["agents"]["builder"]["harness"] = json!(harness);
    fixture.project["mail"] = json!({
        "address": "{{ agent.id }}@example.test",
        "trust": ["human@example.test"]
    });
    copy_sample_harness_layer(&fixture.root);
    let fake = fixture.root.join("fake-harness");
    fs::write(
        &fake,
        r#"#!/bin/sh
set -eu
resume=false
json=false
if [ "$AGENTS_TEST_HARNESS" = opencode ]; then
    test "$1" = --standalone
    test -z "${OPENCODE_DB+set}"
    if [ "$2" = api ]; then
        test "$3" = get
        printf 'checked %s\n' "${4##*/}" >> checked-sessions
        test -f "${4##*/}.session"
        exit
    fi
    test "$2" = run
fi
while [ "$#" -gt 0 ]; do
    case "$1" in
        --session-id) id="$2"; resume=false; shift ;;
        --resume) id="$2"; resume=true; shift ;;
        --session) id="$2"; shift; test ! -f "$id.session" || resume=true ;;
        resume) id="$2"; resume=true; shift ;;
        --json) json=true ;;
        --no-session-persistence|--ephemeral|--last|--continue) exit 9 ;;
    esac
    shift
done
if [ "$AGENTS_TEST_HARNESS" = codex ]; then
    if [ "$resume" = true ]; then
        test "$json" = false
    else
        test "$json" = true
        id=12345678-1234-4234-8234-123456789000
        if [ -f "$id.session" ]; then
            id=12345678-1234-4234-8234-123456789001
        fi
        printf '{"type":"thread.started","thread_id":"not-a-uuid"}\n'
        printf '{"type":"thread.started","thread_id":"%s"}\n' "$id"
    fi
fi
if [ "$resume" = true ]; then
    test -f "$id.session"
else
    test ! -f "$id.session"
    touch "$id.session"
fi
cat
printf '\nfinished builder %s %s\n' "$id" "$resume"
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        fixture.root.join(".agents.d/99-test.json"),
        json!({"harnesses": {(harness): {"program": fake, "env": {"AGENTS_TEST_HARNESS": harness}}}}).to_string(),
    )
    .unwrap();
    fixture.save();

    let deliver = |sender: &str, subject: &str, thread_headers: &str| {
        local_message(
            &fixture.root,
            "builder@example.test",
            sender,
            &format!("From: {sender}\r\nSubject: {subject}\r\n{thread_headers}\r\nHello\r\n"),
        );
    };
    deliver(
        "human@example.test",
        "Before startup",
        "Message-ID: <original@example.test>\r\n",
    );
    let log = fixture.root.join("service.log");
    let start = || {
        Service(
            fixture
                .command()
                .stdin(Stdio::null())
                .stdout(
                    fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log)
                        .unwrap(),
                )
                .spawn()
                .unwrap(),
        )
    };
    let mut service = start();
    let wait_for = |service: &mut Service, count| {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let text = fs::read_to_string(&log).unwrap();
            assert!(service.0.try_wait().unwrap().is_none());
            if text.matches("finished builder").count() >= count {
                return text;
            }
            assert!(Instant::now() < deadline, "{text}");
            thread::sleep(Duration::from_millis(50));
        }
    };
    let sessions = |text: &str| {
        text.lines()
            .filter_map(|line| line.strip_prefix("finished builder "))
            .map(|line| line.to_owned())
            .collect::<Vec<_>>()
    };
    let first = wait_for(&mut service, 1);
    assert!(first.contains("Before startup"));
    let initial = sessions(&first).pop().unwrap();
    let session_id = initial.strip_suffix(" false").unwrap();
    let mut sender = fixture
        .command()
        .args([
            "mail",
            "send",
            "--agent",
            "reviewer",
            "--to",
            "builder",
            "--subject",
            "While watching",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(sender.stdin.take().unwrap(), "New queued request").unwrap();
    assert!(sender.wait().unwrap().success());
    let second = wait_for(&mut service, 2);
    assert!(second.contains("While watching"));
    assert_ne!(sessions(&second)[1], initial);
    assert!(sessions(&second)[1].ends_with(" false"));
    let listed = fixture
        .command()
        .args(["mail", "list", "--agent", "builder", "--unread-only"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let messages = listed["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2, "runs must not mark mail read themselves");
    for message in messages {
        let mail = &message["mail"];
        let output = fixture
            .command()
            .args([
                "mail",
                "set-read",
                "--agent",
                "builder",
                &mail["uid"].to_string(),
                "--uid-validity",
                &mail["uid_validity"].to_string(),
                "read",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    drop(service);
    deliver(
        "human@example.test",
        "Reply from the original thread",
        "Message-ID: <reply@example.test>\r\nIn-Reply-To: <original@example.test>\r\n",
    );
    let mut service = start();
    let reply = wait_for(&mut service, 3);
    assert!(reply.contains("Reply from the original thread"));
    assert_eq!(sessions(&reply)[2], format!("{session_id} true"));
    if harness == "opencode" {
        assert!(session_id.starts_with("ses_"));
        assert_eq!(
            fs::read_to_string(fixture.root.join("checked-sessions")).unwrap(),
            format!("checked {session_id}\n")
        );
        fs::remove_file(fixture.root.join(format!("{session_id}.session"))).unwrap();
        deliver(
            "human@example.test",
            "Reply whose native session was deleted",
            "Message-ID: <missing@example.test>\r\nIn-Reply-To: <original@example.test>\r\n",
        );
    }
    deliver("stranger@example.test", "Not authorized", "");
    thread::sleep(Duration::from_millis(2400));
    let text = fs::read_to_string(log).unwrap();
    assert_eq!(text.matches("finished builder").count(), 3, "{text}");
    assert!(!text.contains("finished reviewer") && !text.contains("Not authorized"));
    if harness == "opencode" {
        assert!(!text.contains("Reply whose native session was deleted"));
        assert_eq!(
            fs::read_to_string(fixture.root.join("checked-sessions"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}

#[test]
fn local_mbox_delivers_reads_filters_and_replies_without_a_server() {
    let mut fixture = Fixture::new();
    fixture.project["mail"] = json!({
        "address": "{{ agent.id }}@example.test",
        "trust": ["human@example.test"]
    });
    fixture.project["agents"]["builder"] = json!({
        "harness":"review", "description":"Build", "instructions":"Build.",
        "mail": {"from": ["reviewer"]}
    });
    fixture.save();
    let mail = |args: &[&str], input: &str| {
        let mut child = fixture
            .command()
            .arg("mail")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let checked = |args: &[&str], input: &str| {
        let output = mail(args, input);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let json = |args: &[&str], input: &str| {
        serde_json::from_slice::<Value>(&checked(args, input)).unwrap()
    };
    assert_eq!(
        json(
            &["send", "--to", "builder", "--subject", "Work"],
            "First line\nFrom body text"
        )
        .get("queued"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        json(&["pending"], "")["queued"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        json(&["list", "--agent", "builder"], "")["messages"],
        json!([])
    );
    checked(&["flush"], "");
    assert_eq!(json(&["pending"], "")["queued"], json!([]));
    checked(&["flush"], ""); // Must not deliver the already-sent queue entry again.
    let found = json(&["search", "--agent", "builder", "From body text"], "");
    assert_eq!(found["messages"].as_array().unwrap().len(), 1);
    let reference = &found["messages"][0]["mail"];
    let uid = reference["uid"].as_u64().unwrap().to_string();
    let validity = reference["uid_validity"].as_u64().unwrap().to_string();
    let full = json(
        &[
            "get",
            "--agent",
            "builder",
            "--uid-validity",
            &validity,
            &uid,
        ],
        "",
    );
    assert!(full["body"].as_str().unwrap().contains("\nFrom body text"));
    assert!(
        !mail(&["get", "--uid-validity", &validity, &uid], "")
            .status
            .success()
    );
    checked(
        &[
            "set-read",
            "--agent",
            "builder",
            "--uid-validity",
            &validity,
            &uid,
            "read",
        ],
        "",
    );
    assert_eq!(
        json(&["list", "--agent", "builder", "--unread-only"], "")["messages"],
        json!([])
    );
    checked(
        &[
            "reply",
            "--agent",
            "builder",
            "--uid-validity",
            &validity,
            &uid,
        ],
        "Done",
    );
    checked(&["flush"], "");
    let reply = json(&["list"], "");
    assert_eq!(reply["messages"][0]["subject"], "Re: Work");

    for sender in ["human@example.test", "stranger@example.test"] {
        local_message(
            &fixture.root,
            "builder@example.test",
            sender,
            "From: human@example.test\r\nSubject: Note\r\n\r\nHuman note\r\n",
        );
    }
    let listed = json(&["search", "--agent", "builder", "Human note"], "");
    assert_eq!(
        listed["messages"].as_array().unwrap().len(),
        1,
        "only trusted humans are visible"
    );
    local_message(
        &fixture.root,
        "builder@example.test",
        "builder@example.test",
        "From: builder@example.test\r\nSubject: Excluded agent\r\n\r\nExcluded agent\r\n",
    );
    assert_eq!(
        json(&["search", "--agent", "builder", "Excluded agent"], "")["messages"],
        json!([])
    );
    assert!(
        !mail(
            &[
                "deliver",
                "--recipient",
                "builder@example.test",
                "--authenticated-from",
                "human@example.test"
            ],
            "X-Agents-Authenticated-From: reviewer@example.test\r\nSubject: Forged\r\n\r\nHi\r\n"
        )
        .status
        .success()
    );
    let local = fixture.root.join(".agents/mail");
    for file in ["inbox.mbox", "outbox.mbox"] {
        assert!(fs::read(local.join(file)).unwrap().starts_with(b"From "));
    }
    assert_eq!(
        fs::metadata(local).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn mail_address_cannot_select_multiple_agents() {
    let mut fixture = Fixture::new();
    fixture.project["mail"]["workspace"] = json!(&fixture.root);
    fixture.project["agents"]["builder"] =
        json!({"harness":"review","description":"Build","instructions":"Build it."});
    fixture.project["agents"]["builder"]["mail"] = json!({"address": "shared@example.test"});
    fixture.project["agents"]["reviewer"]["mail"] = json!({"address": "shared@EXAMPLE.TEST"});
    fixture.save();
    let output = fixture
        .command()
        .args(["mail", "addresses"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("multiple agents registered"));
}

#[test]
fn mail_address_templates_are_project_local_and_can_be_overridden() {
    let mut fixture = Fixture::new();
    fixture.project["mail"]["workspace"] = json!(fixture._dir.path());
    fixture.project["mail"]["address"] = json!(
        "{{ project.path | replace('/', '-') | replace(' ', '-') }}+{{ agent.id }}@example.test"
    );
    fixture.save();

    let project = fixture._dir.path().join("services/website");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(".agents.json"),
        json!({
            "version": 1, "agent": "reviewer",
            "agents": {
                "reviewer": {"harness": "review", "description": "Review", "instructions": "Review."},
                "builder": {"harness": "review", "description": "Build", "instructions": "Build.",
                            "mail": {"address": "{{ agent.id }}@override.test"}}
            },
            "mail": {"address": "{{ project.name }}+{{ agent.id }}@project.test"}
        })
        .to_string(),
    )
    .unwrap();

    let list = |fixture: &Fixture| {
        fixture
            .command()
            .args(["mail", "addresses"])
            .output()
            .unwrap()
    };
    let output = list(&fixture);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let identities: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        identities
            .iter()
            .map(|item| item["address"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "builder@override.test",
            "project-with-spaces+reviewer@example.test",
            "website+reviewer@project.test"
        ]
    );

    fixture.project["mail"]["address"] = json!("not an address");
    fixture.save();
    let output = list(&fixture);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid mail address"));
}
