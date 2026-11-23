use crate::{
    config::{AgentMailbox, Live, Snapshot},
    mail::{self, MailAccess, SessionBusy, request::MailRef},
    types::Id,
};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    fs, io,
    path::Path,
    sync::{
        Arc,
        mpsc::{self, RecvTimeoutError, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

/// How often local mailboxes and configuration files are checked for changes.
const TICK: Duration = Duration::from_secs(1);
/// Upper bound between outbox flushes, so retries come due without other activity.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const COMPACT_INTERVAL: Duration = Duration::from_secs(3600);
/// Pause after a worker error before scanning an unchanged inbox again.
const ERROR_PAUSE: Duration = Duration::from_secs(30);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3600);

/// Receives mail, runs agents for it, and delivers their replies, forever or, with `once`,
/// until the current backlog is handled. Returns whether every step of a `once` pass succeeded.
pub fn serve(once: bool) -> Result<bool> {
    let live = Arc::new(Live::load(None)?);
    if once {
        return Ok(serve_once(&live.get()));
    }
    eprintln!("agents: watching {}", live.get().workspace.root().display());
    serve_forever(&live)
}

fn serve_once(snapshot: &Snapshot) -> bool {
    let mut ingress = mail::ImapIngress::default();
    let mut ok = report(
        "receive",
        ingress.receive(&snapshot.workspace, &snapshot.routes),
    );
    ok &= flush(snapshot);
    ok &= thread::scope(|scope| {
        let workers = mailboxes(snapshot)
            .map(|(id, mailbox)| {
                scope.spawn(move || {
                    let result =
                        MailAccess::new(snapshot.workspace.mail(), &snapshot.routes, mailbox)
                            .and_then(|access| {
                                let mut ok = true;
                                for message in access.pending_runs()? {
                                    ok &= run(snapshot, &access, id, mailbox, message)?;
                                }
                                Ok(ok)
                            });
                    result.unwrap_or_else(|error| {
                        eprintln!("agents: {id}: {error:#}");
                        false
                    })
                })
            })
            .collect::<Vec<_>>();
        workers.into_iter().fold(true, |ok, worker| {
            worker.join().expect("agent worker panicked") & ok
        })
    });
    ok & flush(snapshot)
}

struct Worker {
    /// Dropping the sender asks the worker to stop after its current run.
    _stop: mpsc::Sender<()>,
    handle: JoinHandle<()>,
}

fn serve_forever(live: &Arc<Live>) -> Result<bool> {
    let receiver = Arc::clone(live);
    thread::Builder::new()
        .name("agents-ingress".into())
        .spawn(move || receive(&receiver))?;
    let (finished, runs) = mpsc::channel();
    let mut workers = BTreeMap::<Id, Worker>::new();
    let mut flushed: Option<Instant> = None;
    let mut compacted: Option<Instant> = None;
    let mut outbox = None;
    let mut woken = false;
    loop {
        let snapshot = match live.refresh() {
            Ok(Some(next)) => {
                eprintln!("agents: configuration reloaded");
                next
            }
            Ok(None) => live.get(),
            Err(error) => {
                eprintln!("agents: reload configuration: {error:#}");
                live.get()
            }
        };
        reconcile(&mut workers, live, &snapshot, &finished)?;
        let observed = stamp(&outbox_path(&snapshot));
        if woken
            || outbox != Some(observed)
            || flushed.is_none_or(|at| at.elapsed() >= FLUSH_INTERVAL)
        {
            flush(&snapshot);
            flushed = Some(Instant::now());
            outbox = Some(stamp(&outbox_path(&snapshot)));
        }
        if compacted.is_none_or(|at| at.elapsed() >= COMPACT_INTERVAL) {
            report("compact", mail::compact(&snapshot.workspace));
            compacted = Some(Instant::now());
            outbox = Some(stamp(&outbox_path(&snapshot)));
        }
        woken = runs.recv_timeout(TICK).is_ok();
        while runs.try_recv().is_ok() {}
    }
}

/// Starts a worker for each agent mailbox in the project and stops workers for removed agents.
fn reconcile(
    workers: &mut BTreeMap<Id, Worker>,
    live: &Arc<Live>,
    snapshot: &Snapshot,
    finished: &mpsc::Sender<()>,
) -> Result<()> {
    let wanted: Vec<Id> = mailboxes(snapshot).map(|(id, _)| id.clone()).collect();
    workers.retain(|id, worker| wanted.contains(id) && !worker.handle.is_finished());
    for id in wanted {
        if workers.contains_key(&id) {
            continue;
        }
        let (stop, stopped) = mpsc::channel();
        let (live, finished, agent) = (Arc::clone(live), finished.clone(), id.clone());
        let handle = thread::Builder::new()
            .name(format!("agents-{id}"))
            .spawn(move || work(&live, &agent, &stopped, &finished))?;
        workers.insert(
            id,
            Worker {
                _stop: stop,
                handle,
            },
        );
    }
    Ok(())
}

/// Runs the agent `id` for each message that arrives in its inbox, one at a time.
fn work(live: &Live, id: &Id, stopped: &mpsc::Receiver<()>, finished: &mpsc::Sender<()>) {
    let mut scanned: Option<(Arc<Snapshot>, Stamp)> = None;
    loop {
        let snapshot = live.get();
        let Some((_, mailbox)) = mailboxes(&snapshot).find(|(agent, _)| *agent == id) else {
            return;
        };
        let mut pause = TICK;
        let result = MailAccess::new(snapshot.workspace.mail(), &snapshot.routes, mailbox)
            .and_then(|access| {
                let observed = stamp(&access.inbox_path());
                if scanned
                    .as_ref()
                    .is_some_and(|(seen, stamp)| Arc::ptr_eq(seen, &snapshot) && *stamp == observed)
                {
                    return Ok(());
                }
                scanned = Some((Arc::clone(&snapshot), observed));
                for message in access.pending_runs()? {
                    if matches!(stopped.try_recv(), Err(TryRecvError::Disconnected)) {
                        return Ok(());
                    }
                    run(&snapshot, &access, id, mailbox, message)?;
                    // Wake the main loop to deliver the replies.
                    let _ = finished.send(());
                }
                Ok(())
            });
        if let Err(error) = result {
            eprintln!("agents: {id}: {error:#}");
            scanned = None;
            pause = ERROR_PAUSE;
        }
        if stopped.recv_timeout(pause) != Err(RecvTimeoutError::Timeout) {
            return;
        }
    }
}

/// Runs the agent once for `message`; returns whether the run succeeded.
/// The message is marked dispatched first, so a crash or restart never runs it twice.
fn run(
    snapshot: &Snapshot,
    access: &MailAccess<'_>,
    id: &Id,
    mailbox: &AgentMailbox,
    message: MailRef,
) -> Result<bool> {
    let project = snapshot.workspace.project(Some(id))?;
    let timeout = project.timeout.unwrap_or(DEFAULT_TIMEOUT);
    access.dispatch(message)?;
    eprintln!("agents: routing mail {} to {id}", message.uid);
    let mail = snapshot.workspace.mail();
    match mail::execute(project, mail, &snapshot.routes, mailbox, message, timeout) {
        Ok(status) if status.success() => return Ok(true),
        Ok(status) => eprintln!("agents: {id}: harness {status}"),
        Err(error) if error.is::<SessionBusy>() => {
            // Another run holds the conversation; try again once it finishes.
            access.undispatch(message)?;
            return Ok(true);
        }
        Err(error) => eprintln!("agents: {id}: {error:#}"),
    }
    access.fail(message)?;
    Ok(false)
}

/// Imports incoming IMAP mail until the process exits.
fn receive(live: &Live) {
    let mut ingress = mail::ImapIngress::default();
    let mut current: Option<Arc<Snapshot>> = None;
    loop {
        let snapshot = live.get();
        if current
            .as_ref()
            .is_some_and(|current| !Arc::ptr_eq(current, &snapshot))
        {
            ingress.reset();
        }
        report(
            "receive",
            ingress.receive(&snapshot.workspace, &snapshot.routes),
        );
        ingress.wait(snapshot.workspace.mail().poll_interval());
        current = Some(snapshot);
    }
}

fn mailboxes(snapshot: &Snapshot) -> impl Iterator<Item = (&Id, &AgentMailbox)> {
    snapshot.routes.project_mailboxes(snapshot.workspace.root())
}

fn flush(snapshot: &Snapshot) -> bool {
    match mail::flush(&snapshot.workspace) {
        Ok(delivered) => delivered,
        Err(error) => {
            eprintln!("agents: flush: {error:#}");
            false
        }
    }
}

fn report(step: &str, result: Result<()>) -> bool {
    result
        .inspect_err(|error| eprintln!("agents: {step}: {error:#}"))
        .is_ok()
}

fn outbox_path(snapshot: &Snapshot) -> std::path::PathBuf {
    let workspace = &snapshot.workspace;
    workspace
        .root()
        .join(workspace.mail_directory())
        .join("outbox.mbox")
}

type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    match fs::metadata(path) {
        Ok(metadata) => Some((
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            metadata.len(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => Some((SystemTime::UNIX_EPOCH, u64::MAX)),
    }
}
