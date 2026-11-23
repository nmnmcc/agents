---
default: major
---

# Exchange standard signed mail and keep run state on messages

External mail is now ordinary signed mail. Send OpenPGP/MIME or S/MIME
mail from any client to an Agent's `+tag` address; every local Agent in the
signed `To` or `Cc` gets a copy, and protected headers take precedence over
the outer ones. Outgoing mail is standard `multipart/signed` mail with
protected headers that other mail clients verify.

The service runs a worker for each Agent, so Agents handle mail in parallel.
It waits for IMAP mail with IDLE where supported and reloads the project
configuration when its files change.

Migration:

- Signed mail no longer uses `Agent-To:` or `Task-ID:` inside the signed
  part, and clearsigned inline OpenPGP is no longer accepted. Address the
  Agent in `To` or `Cc` instead.
- The IMAP source folder is now modified: every imported or rejected
  message is marked `\Seen`, and rejected ones also get `AgentsRejected`.
  On the first start, unseen mail already in the folder is processed once.
- A message starts at most one run, even across restarts. Runs are marked
  `AgentsDispatched`, failed or timed-out runs `AgentsFailed`. Use
  `agents mail retry` to run an Agent again for a message.
- New options: `agents.<id>.timeout` (default 3600 seconds),
  `mail.transport.imap.poll_interval` (default 60), `mail.transport.signatures.max_age`
  (default 7 days) and `mail.transport.retry.retain` (default 7 days).
- `agents mail flush` exits with a failure when some mail was not
  delivered. `agents mail deliver --unsigned` no longer requires
  `--recipient`.
