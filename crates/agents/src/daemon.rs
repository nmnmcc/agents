use crate::{
    config::{AgentMailbox, MailRoutes, Workspace},
    mail::{self, MailAccess, SessionBusy, request::MailRef},
    types::Id,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    thread,
    time::Duration,
};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Flushes the outbox and runs agents for unread mail, forever or, with `once`, a single time.
/// Returns whether every delivery and run in the last pass succeeded.
pub fn serve(once: bool) -> Result<bool> {
    let workspace = Workspace::load(None)?;
    let routes = workspace.mail_routes()?;
    let mut ingress = mail::ImapIngress::default();
    let mut observed = BTreeMap::<Id, BTreeSet<MailRef>>::new();
    if !once {
        eprintln!("agents: watching {}", workspace.root().display());
    }
    loop {
        let mut ok = true;
        if let Err(error) = ingress.poll(&workspace, &routes) {
            eprintln!("agents: receive: {error:#}");
            ok = false;
        }
        if let Err(error) = mail::flush(&workspace) {
            eprintln!("agents: flush: {error:#}");
            ok = false;
        }
        for (id, mailbox) in routes.project_mailboxes(workspace.root()) {
            let observed = observed.entry(id.clone()).or_default();
            match run_unread(&workspace, &routes, id, mailbox, observed) {
                Ok(succeeded) => ok &= succeeded,
                Err(error) => {
                    eprintln!("agents: {id}: {error:#}");
                    ok = false;
                }
            }
        }
        if once {
            return Ok(ok);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Runs the agent once for each unread message not yet observed; returns whether all succeeded.
fn run_unread(
    workspace: &Workspace,
    routes: &MailRoutes,
    id: &Id,
    mailbox: &AgentMailbox,
    observed: &mut BTreeSet<MailRef>,
) -> Result<bool> {
    let unread = MailAccess::new(workspace.mail(), routes, mailbox)?.unread_refs()?;
    let arrived: Vec<_> = unread.difference(observed).copied().collect();
    // Do not mark mail read, or spawn repeatedly on unchanged mail if the run fails.
    *observed = unread;
    let mut succeeded = true;
    for message in arrived {
        eprintln!("agents: routing unread mail {} to {id}", message.uid);
        let result = workspace
            .project(Some(id))
            .and_then(|project| mail::execute(project, workspace.mail(), routes, mailbox, message));
        match result {
            Ok(status) if status.success() => continue,
            Ok(status) => eprintln!("agents: {id}: harness {status}"),
            Err(error) if error.is::<SessionBusy>() => {
                observed.remove(&message);
                continue;
            }
            Err(error) => eprintln!("agents: {id}: {error:#}"),
        }
        succeeded = false;
    }
    Ok(succeeded)
}
