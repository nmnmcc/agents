# agents

Run project agents with existing harness CLIs. Let them talk by email.

**Just use mail.** An agent has an address, not an RPC endpoint. A request
can wait in an inbox; a human can join the same thread from a mail client.
Use To, Cc, Bcc, and replies instead of inventing a coordination protocol.

## Watch a project's mail

```sh
cd /path/to/project
agents
```

With no arguments, `agents` runs in the foreground. It flushes outgoing mail
and starts each recipient agent when its inbox has unread mail at startup
or new unread mail arrives. It runs one agent at a time. Stop it with
`Ctrl-C`. Set up [mail addresses](#exchange-mail-locally) and install and
authenticate the harnesses your agents use.

An unchanged unread message does not trigger repeated runs in the same
service process, even if the harness fails. Restarting the service checks
all unread mail again and reloads the project configuration. The service
does not mark messages read or turn stdout into a reply.

The first message starts a native agent session. A reply in that thread
resumes the same session for that agent, even after restarting `agents`.
The association is stored in the project's private mail directory; the
harness keeps the session itself. Replies to mail sent by the agent's
`agents-mail` tools also resume the sending session. Wire headers identify
messages, not native session IDs, and a session link is scoped to an
authenticated participant.

The watcher requires a session-capable adapter. `agents` supplies a session
ID and treats every harness the same way; each difference lives in its
adapter. The Claude Code, Codex, and OpenCode V2 samples are all
session-capable. Codex assigns its own thread ID, so its adapter declares
`sessions: reported` and a `stdout` hook reads the ID from the JSON event
stream. OpenCode would start a new session for an unknown ID, so its adapter
checks with a `before` hook before resuming. If an existing session cannot be resumed, the run fails rather
than starting a new one.

### Run it as a user service

From the project directory:

```sh
agents service          # Print a systemd unit (Linux) or LaunchAgent plist (macOS)
agents service install  # Install, enable, and start it for the current user
```

Use `--platform systemd` or `--platform launchd` to print a specific
format. Installation needs no `sudo`. It uses the systemd user manager
on Linux and the logged-in user's `~/Library/LaunchAgents` on macOS.
The definition uses the current executable's absolute path and the
project root. Install from a stable binary location, and stop any
foreground instance before installing.

Services do not inherit your interactive shell's environment. Make sure
the service can find the harness (or configure an absolute `program`
path) and access its credentials. The generated definition does not copy
shell variables or secrets.

## Run a task without mail

From this repository:

```sh
cargo install --locked --path crates/agents
agents config check samples/project
agents config render samples/project
printf '%s\n' 'Review this project for missing tests.' |
  agents run samples/project
```

`run` reads the task from stdin, forwards the harness's stdout and stderr,
and returns its exit status. Only `run` needs an installed and authenticated
harness. The example uses
Claude Code; change the agent's `harness` to `codex` or `opencode` to use
another. `config render` previews the command and temporary files as JSON
without launching the harness. In your own project, omit the directory to
use the current one; pass `--agent ID` to override the default.

## Exchange mail locally

In a project with `builder` and `reviewer` agents, give both an address:

```yaml
mail:
  address: "{{ agent.id }}@example.test"
```

These addresses are local routes; no DNS, SMTP, or IMAP server is needed
when both ends use local storage.

```sh
agents mail addresses
printf '%s\n' 'Please review commit abc123.' |
  agents mail send --agent builder --to reviewer --subject 'Review abc123'
agents mail flush
agents mail list --agent reviewer --unread-only
printf '%s\n' 'Review the unread request in your inbox.' |
  agents run --agent reviewer
```

`send` queues a message; the foreground service delivers it and starts the
recipient. The example above shows manual delivery and a one-off run
instead. Local mail is stored in `.agents/mail/inbox.mbox` and
`.agents/mail/outbox.mbox`; keep their index and flag sidecars with them.
Set `mail.directory` to use another private directory.

Use agent IDs for recipients. For separate projects, set `mail.workspace`
to a common directory, give agents unique addresses, and use selectors
like `services/api:reviewer`, relative to that directory. Override an
individual address with `agents.<id>.mail.address`.

## Read, search, and reply

```sh
agents mail search --agent reviewer 'abc123'
# Substitute the uid and uid_validity returned by list or search.
agents mail thread --agent reviewer 42 --uid-validity 1001
printf '%s\n' 'Reviewed; the tests cover this change.' |
  agents mail reply --agent reviewer 42 --uid-validity 1001 --all
agents mail flush
```

The CLI returns JSON for mailbox queries and works without a harness.
Use `--directory` to select a different project. `agents mail --help` lists
all operations, including `get`, `attachment`, and `set-read`.

On any run, a mail-enabled agent gets a short unread summary as
untrusted context, separate from the task. The per-run `agents-mail` MCP
server provides the same mailbox operations: list, search, get, thread,
attachment, read state, send, and reply. Reading does not mark mail read.

Repeat `--to`, `--cc`, or `--bcc` for multiple recipients. Replies preserve
the thread and use `Reply-To` before `From`, subject to authorization.
`--all` also includes visible To/Cc recipients, excluding self and
duplicates; it never restores Bcc. Blind recipients do not appear in
delivered headers.

## Use a saved message as a task

Inspect a saved RFC 5322 message before deciding whether to run it. Supply
the envelope recipient separately; the `To` header does not select the agent.

```sh
agents mail inspect --recipient reviewer@example.test < request.eml
agents mail run --recipient reviewer@example.test < request.eml
```

`inspect` shows the selected agent and decoded plain-text task. Run the
second command only after approving that text as a task. It does not reply.

## Exchange mail with people

Use addresses your mail server routes to and authorizes for submission.
Keep local mbox storage and configure `mail.transport.smtp` for outgoing
mail, or also set `mail.transport.imap` and `mail.transport.outbox` to use
external storage. See the [mail sample](samples/mail.agents.json) for SMTP.
For IMAP, use a dedicated inbox and private outbox; the outbox must support
permanent custom keywords.

With local storage, have a trusted gateway deposit incoming mail:

```sh
agents mail deliver --recipient reviewer@example.test \
  --authenticated-from maintainer@example.test < message.eml
```

`agents` listens on no mail ports and does not authenticate mail itself.
The gateway must verify the sender independently; `From`, an SMTP
envelope sender, or SPF alone is not an authenticated mailbox identity.
Strip all incoming `X-Agents-Envelope-To` and `X-Agents-Authenticated-From`
headers. `deliver` rejects messages containing either header and stamps
its own. A gateway writing directly to IMAP must stamp exactly one of
each itself; never use an inbox where untrusted parties can write them.

- `mail.from` and `mail.to` restrict agent senders and recipients by ID.
  Agent-level rules can narrow project rules. Omission means unrestricted;
  `[]` permits no agents in that direction.
- `mail.trust` lists permitted human addresses. Without it, agents can
  read authenticated human mail and reply to the authenticated sender,
  but cannot start new mail to arbitrary human addresses.
- `mail.cc` and `mail.bcc` explicitly authorize project-wide copies of
  outgoing mail.

The foreground service flushes automatically. If it is not running, run
`agents mail flush` periodically. Use only one flusher per outbox.
`agents mail pending` shows queued, rejected, and expired deliveries.
Only a final SMTP `250` counts as success; `5xx` ends that delivery,
while `4xx` or a lost acknowledgment is retried with the same `Message-ID`.
A lost acknowledgment can cause a duplicate. Retries are per recipient,
use [Postfix-style backoff](https://www.postfix.org/qmgr.8.html), and expire
after five days by default. A successful `send` means queued, not delivered.

## Project configuration

The [sample project](samples/project) includes an
[adapter layer](samples/project/.agents.d/00-harnesses.yaml)
and a skill under `.agents/skills/`. Copy the project, including its dotfiles,
to use it as a starting point. For a two-agent mail setup, use
[`samples/mail.agents.json`](samples/mail.agents.json) as your `.agents.json`
and keep the adapter layer. Replace or remove the sample MCP and mail endpoints
and adjust the addresses. Samples are templates; `agents` only loads
configuration from your project and does not fall back to the samples.
Use the [generated schema](agents.schema.json) for
editor completion (adjust the schema paths after copying) and
`agents config check` to validate the merged settings.

Regenerate the schema from the repository root:

```sh
cargo run --locked -q -p agents -- config schema > agents.schema.json
```

- The nearest project wins. Use one `.agents.json`, `.agents.jsonc`,
  `.agents.json5`, `.agents.yaml`, or `.agents.yml`, optionally followed by
  `.agents.d/` layers in filename order. A directory can also use layers
  alone. Objects merge; arrays and scalars replace. Projects do not inherit
  each other's layers.
- Top-level `agent` selects the default. Project `instructions` fall back
  to `AGENTS.md` when omitted; explicit instructions replace that fallback.
  Each agent has separate instructions and selects a `harness` and optional
  `model`. `agents.default`, `mcp.default`, and `harnesses.default` fill
  missing fields; they do not define entries.
- Skills are discovered from `.agents/skills/<name>/SKILL.md`. Top-level
  `skills` adds or overrides paths. An agent can select a subset or use
  `skills: []` to opt out.
- An agent's `mcp` list selects servers by ordered patterns, such as
  `["*", "!experimental-*"]`; last match wins. `mcp.<id>.disable: true`
  overrides selection.
- Use `{ "env": "VARIABLE" }` or `{ "file": "path" }` for external string
  values. Relative file paths start at the project root. Render previews
  withhold plans containing resolved references.

Harness adapters are project configuration, not separate installations.
Each defines a program, arguments, capabilities, and any temporary files.
Arguments are rendered individually, without a shell. Arguments,
environment overrides, and generated files apply only to that invocation;
`agents` does not rewrite global or harness settings. Run only trusted
projects and adapters: `agents` is not a sandbox.

Every templated value (arguments, environment overrides, generated files,
`stdin`, hook commands, and mail addresses) is either a
MiniJinja string or `{ "js": "..." }`. JavaScript runs in a fresh QuickJS
context with no I/O and with memory, stack, and time limits. The template
context is available as global variables, and the value of the last statement
is the result. An argument script may return an array of arguments or `null`
for none, which is how optional arguments are written; an environment script may return `null` to remove the variable; a
`json` file script may return any JSON value.

```yaml
env:
  OPENCODE_DB: { js: 'session === null ? ":memory:" : null' }
```

Hooks adapt a harness without changing `agents`:

- `hooks.before` and `hooks.after` are commands, each an argument list whose
  first element is the program (`program` names the harness). They run in
  order in the project directory with the harness environment; an empty list
  is skipped. A failing `before` command aborts the run, and a failing
  `after` command fails it. `after` commands also see the exit code as
  `status`, or `null` after a signal.
- `hooks.stdout` and `hooks.stderr` are scripts run on each output line, as
  `line`, with `stream` naming the stream. Return `null` to pass the line
  through, or an object: `line` replaces it (`null` drops it) and `session`
  reports a native session ID. `const` and `let` last one line; state kept on
  `globalThis` lasts the whole run.
- `capabilities.sessions` is `none`, `supplied` (the harness uses the ID
  `agents` supplies), or `reported` (an output hook reports the harness's own
  ID for a new session).

## Development

```sh
devenv shell -- treefmt                       # Format supported files
devenv shell -- treefmt --ci                  # Fail if formatting changes files
devenv shell -- cargo test --locked --workspace
devenv shell -- test-e2e                     # Live IMAP/SMTP round trip
devenv test                                  # Run both Rust tests and E2E
```

Run the E2E test as a regular user. It builds the locked workspace, starts
Dovecot and a test SMTP server on loopback, and uses a fake harness. It needs
no Docker, root privileges, real mail accounts, or harness login.

Source layout, under `crates/agents/src/`:

- `cli/`: argument parsing and stdin/stdout; no logic of its own.
- `config/`: project discovery, layer merging, value references, mail routing,
  and the JSON Schema.
- `harness.rs`: renders an adapter into a plan and launches it.
- `template.rs`: MiniJinja and sandboxed QuickJS evaluation of configured
  values.
- `mail/`: mailboxes (local mbox or IMAP), the outbox and its retries, native
  session bindings, and the per-run `agents-mail` MCP bridge.
- `daemon.rs`: the default command; polls mail and calls the same runner as
  `agents run`.
- `service.rs`: user service definitions for systemd and launchd.
- `types.rs`: validated string types shared by all of the above.

Integration tests live in `tests/cli/`; the live mail test is
`tests/mail_e2e.py`.
