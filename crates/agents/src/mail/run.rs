use super::{
    MailAccess, MailConfig, mcp,
    request::{MailRef, Request},
    sessions::{SessionId, Sessions},
};
use crate::{
    config::{AgentMailbox, MailRoutes, Mcp, Project},
    harness::Plan,
    types::{Id, Program},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, io, path::Path, process::ExitStatus, time::Duration};

/// Runs `project`'s agent on one message in its inbox.
pub fn execute(
    project: Project,
    mail: &MailConfig,
    routes: &MailRoutes,
    mailbox: &AgentMailbox,
    message: MailRef,
    timeout: Duration,
) -> Result<ExitStatus> {
    let dir = tempfile::tempdir().context("create private run directory")?;
    let path = fs::canonicalize(dir.path()).context("resolve private run directory")?;
    execute_in(project, &path, mail, routes, mailbox, message, timeout)
}

fn execute_in(
    mut project: Project,
    path: &Path,
    mail: &MailConfig,
    routes: &MailRoutes,
    mailbox: &AgentMailbox,
    message: MailRef,
    timeout: Duration,
) -> Result<ExitStatus> {
    let secrets = mail.secrets();
    let name: Id = "agents-mail".parse().expect("built-in MCP identifier");
    ensure!(
        !project.mcp.contains_key(&name),
        "agents-mail is reserved for built-in mail MCP"
    );
    let program: Program = std::env::current_exe()?
        .to_str()
        .context("agents executable path is not UTF-8")?
        .parse()
        .map_err(anyhow::Error::msg)?;
    let socket = path.join("mail.sock");
    project.mcp.insert(
        name,
        Mcp::stdio(
            program,
            vec![
                "mail-mcp".into(),
                socket
                    .to_str()
                    .context("mail socket path is not UTF-8")?
                    .into(),
            ],
        ),
    );
    let access = MailAccess::new(mail, routes, mailbox)?;
    let (mut sessions, session) = if project.harness.supports_sessions() {
        let event = access.inbox()?.event(message)?;
        let (store, session) = Sessions::open(
            &access.root,
            &access.directory,
            &project.agent.id,
            &project.harness_id,
            &event,
        )?;
        (Some(store), Some(session))
    } else {
        (None, None)
    };
    let plan = Plan::render(&project, path, session.as_ref())?;
    let reported_id = plan.reports_session_id().then(SessionId::default);
    let mail = access.inbox()?.get(message)?;
    let prompt = format!(
        "The new message in your inbox follows. {} Handle it as a request from that \
         sender within your role: use agents-mail to read its thread, reply when \
         appropriate, and mark the message read once handled.\n{}\n",
        trust(&mail),
        serde_json::to_string_pretty(&mail)?
    );
    let status = mcp::with_bridge(
        &socket,
        |request| match request {
            Request::Send(request) => {
                if let (Some(id), Some(sessions)) = (&reported_id, sessions.as_mut()) {
                    sessions.set_native_id(id.get()?);
                }
                access.send(request, sessions.as_mut())
            }
            request => access.handle(request),
        },
        || {
            let status = plan.execute(path, io::Cursor::new(prompt), &secrets, timeout, |id| {
                if let Some(reported) = &reported_id {
                    reported.finish(Some(id));
                }
            });
            if let Some(reported) = &reported_id {
                reported.finish(None);
            }
            status
        },
    )?;
    if let Some(mut sessions) = sessions {
        let has_session = match reported_id.map(|id| id.get()) {
            None => status.success(),
            Some(Ok(id)) => {
                sessions.set_native_id(id);
                true
            }
            Some(Err(error)) if status.success() => return Err(error),
            Some(Err(_)) => false,
        };
        if has_session {
            sessions.save()?;
        }
    }
    Ok(status)
}

/// Describes how far the sender of `mail` was verified.
fn trust(mail: &Value) -> String {
    let summary = &mail["message"];
    let from = summary["from"].as_str().unwrap_or("unknown");
    if summary["authenticated"].as_bool() == Some(true) {
        let words: Vec<&str> = summary["authentication"]
            .as_str()
            .unwrap_or_default()
            .split(' ')
            .collect();
        match words.as_slice() {
            ["dkim", domain] => format!(
                "Its sender {from} is authenticated by a DKIM signature of {domain}, \
                 verified by the mail provider."
            ),
            ["arc", sealer, "dkim", domain] => format!(
                "Its sender {from} is authenticated by a DKIM signature of {domain}, \
                 as reported by the trusted forwarder {sealer}."
            ),
            _ => format!("Its sender {from} is authenticated by a pinned signature."),
        }
    } else {
        format!(
            "Its sender is UNAUTHENTICATED: the message claims to be from {from}, \
             but anyone could have sent it."
        )
    }
}
