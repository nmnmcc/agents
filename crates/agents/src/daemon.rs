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

pub fn serve() -> Result<()> {
    let workspace = Workspace::load(None)?;
    let routes = workspace.mail_routes()?;
    let mut observed = BTreeMap::<Id, BTreeSet<MailRef>>::new();
    eprintln!("agents: watching {}", workspace.root().display());
    loop {
        if let Err(error) = mail::flush(&workspace) {
            eprintln!("agents: flush: {error:#}");
        }
        for (id, mailbox) in routes.project_mailboxes(workspace.root()) {
            if let Err(error) = run_if_unread(
                &workspace,
                &routes,
                id,
                mailbox,
                observed.entry(id.clone()).or_default(),
            ) {
                eprintln!("agents: {id}: {error:#}");
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn run_if_unread(
    workspace: &Workspace,
    routes: &MailRoutes,
    id: &Id,
    mailbox: &AgentMailbox,
    observed: &mut BTreeSet<MailRef>,
) -> Result<()> {
    let unread = MailAccess::new(workspace.mail(), routes, mailbox)?.unread_refs()?;
    let arrived: Vec<_> = unread.difference(observed).copied().collect();
    // Do not mark mail read, or spawn repeatedly on unchanged mail if the run fails.
    *observed = unread;
    for message in arrived {
        eprintln!("agents: routing unread mail {} to {id}", message.uid);
        let task = b"Consider the new message in this thread and handle requests appropriate \
                     to your role. Reply in this mail thread when appropriate and mark \
                     the message read once handled.\n";
        let result = workspace.project(Some(id)).and_then(|project| {
            mail::execute(
                project,
                task.as_slice(),
                workspace.mail(),
                Some((routes, mailbox)),
                Some(message),
            )
        });
        match result {
            Ok(status) if status == std::process::ExitCode::SUCCESS => {}
            Ok(_) => eprintln!("agents: {id}: harness exited unsuccessfully"),
            Err(error) if error.is::<SessionBusy>() => {
                observed.remove(&message);
            }
            Err(error) => eprintln!("agents: {id}: {error:#}"),
        }
    }
    Ok(())
}
