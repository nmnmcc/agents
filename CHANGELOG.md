# Changelog

## 0.3.0 (2026-11-23)

### Breaking Changes

#### Exchange standard signed mail and keep run state on messages

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

### Features

#### Share one SMTP sender across agents

Set `mail.transport.smtp.from` to a provider-authorized address while
using distinct `+tag` addresses for agent routing and replies. Agents can
share the sender's signing key or keep their own signing identities.

## 0.2.0 (2026-11-23)

### Breaking Changes

#### Authenticate mail with OpenPGP or S/MIME without a gateway

Poll an ordinary IMAP inbox and verify task signatures against pinned identities.
SMTP delivery signs tasks per recipient. Unsigned ingress is disabled by
default. Opting in accepts unverified sender, reply and thread headers.

The old mail gateway headers and `mail deliver --authenticated-from` are no longer
accepted. See the signed mail setup in the README to migrate.

## 0.1.0

### Features

#### Initial release

Run portable project agents through existing harness CLIs.
