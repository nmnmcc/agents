# agents

Run project agents with existing harness CLIs. Let them talk by email.

**Just use mail.** An agent has an address, not an RPC endpoint. A request
can wait in an inbox; a human can join the same thread from a mail client.
Use To, Cc, Bcc, and replies instead of inventing a coordination protocol.
The only way to give an agent work is to mail it: every run starts from one
message in its inbox.

## Install with Nix

With `nix-command` and `flakes` enabled:

```sh
nix profile install github:nmnmcc/agents
```

To try it without installing, run `nix run github:nmnmcc/agents -- --help`.
From a local checkout, use `nix profile install path:.` to install or
`nix build path:.` to build `result/bin/agents`.
The same package is available through `devenv build outputs.agents`.
Linux builds link statically against musl. macOS builds link third-party
libraries statically and still use the system libraries and frameworks.
Install and authenticate the harness CLIs you plan to use separately.

## Watch a project's mail

```sh
cd /path/to/project
agents
```

With no arguments, `agents` runs in the foreground. It flushes outgoing mail
and runs the recipient agent once for each unread message. Each agent has
its own worker, so agents run in parallel while each one handles its mail in
order. Stop it with `Ctrl-C`. `agents --once` handles the current backlog and
exits, failing if any delivery or run failed; use it from scripts or a
scheduler. Set up [mail addresses](#exchange-mail-locally) and install and
authenticate the harnesses your agents use.

Run state lives on the messages in the private inbox, so it survives
restarts. Before a run starts, the message gets the `AgentsDispatched`
keyword, and the message never starts another run, even if the agent leaves
it unread. A run that fails or times out also gets `AgentsFailed`;
`agents mail list` reports both as `dispatched` and `failed`. To run the agent
again for a message, use `agents mail retry --agent <id> <uid> --uid-validity
<n>`. The service does not mark messages read or turn stdout into a reply;
read state belongs to the agent.

A run, including any processes the harness started, is killed after
`agents.<id>.timeout` seconds (default 3600; set it for every agent in
`agents.default`). `after` hooks still run, with a null `status`.

The agent receives the new message as a prompt on standard input. Customize
it with a template in `mail.prompt`, or per agent in `agents.<id>.mail.prompt`:
MiniJinja text or `{"js": "..."}` like other templates. The context has
`agent` (`id`, `description`, `instructions`), `address` (the agent's mail
address) and `mail` (the message as `agents mail get` prints it). The
`json` filter prints a value as indented JSON. The default is:

```jinja
The new message in your inbox follows. Handle it as a request from its sender within your role: use agents-mail to read its thread, reply when appropriate, and mark the message read once handled.
{{ mail | json }}
```

The service reloads the project configuration when its files change,
including `.agents.d/` layers, referenced files, and the projects in a
`mail.workspace`. Runs already in progress finish with the configuration
they started with. An invalid edit is reported and the previous
configuration stays in effect until the files are fixed.

The first message starts a native agent session. A reply in that thread
resumes the same session for that agent, even after restarting `agents`.
The association is stored in the project's private mail directory; the
harness keeps the session itself. Replies to mail sent by the agent's
`agents-mail` tools also resume the sending session. Wire headers identify
messages, not native session IDs, and a session link is scoped to an
authenticated participant.

Sessions need a session-capable adapter; without one, every message starts
a fresh run, and the thread itself is the only context. `agents` supplies a
session ID and treats every harness the same way; each difference lives in
its adapter. The Claude Code, Codex, and OpenCode V2 samples are all
session-capable. Codex assigns its own thread ID, so its adapter declares
`sessions: reported` and a `stdout` hook reads the ID from the JSON event
stream. OpenCode would start a new session for an unknown ID, so its adapter
checks with a `before` hook before resuming. If an existing session cannot be
resumed, the run fails rather than starting a new one.

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

## Check a project

From this repository:

```sh
cargo install --locked --path crates/agents
agents config check samples/project
agents config render samples/project
```

`config render` previews the harness command and temporary files as JSON
without launching the harness, which needs no installation or login. The
sample uses Claude Code; change an agent's `harness` to `codex` or
`opencode` to use another. In your own project, omit the directory to use
the current one.

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
agents --once
```

`send` queues a message; the watcher delivers it and runs the recipient.
The example above does the same by hand: `flush` delivers, and
`agents --once` runs `reviewer` on its unread request. Local mail is stored in `.agents/mail/inbox.mbox` and
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

Each run gives the agent its triggering message as untrusted data and asks
it to handle the message as its role requires, reply when appropriate, and
mark it read. The per-run `agents-mail` MCP server provides the same mailbox operations: list, search, get, thread,
attachment, read state, send, and reply. Reading does not mark mail read.

Repeat `--to`, `--cc`, or `--bcc` for multiple recipients. Replies to
external signed mail use the configured signer's address, not the outer
`Reply-To` or `From`. For local mail and explicitly enabled unsigned mail,
`Reply-To` takes precedence over `From`, subject to authorization.
`--all` includes visible To/Cc recipients
from the imported message, excluding self and duplicates; it never
restores Bcc. Blind recipients do not appear in delivered headers.

## Exchange mail with people

Use addresses your provider routes to, and an address it authorizes for
SMTP submission.
`agents` watches an ordinary IMAP folder and authenticates external mail with
**OpenPGP or S/MIME**. It does not listen on a mail port or require a
gateway. IMAP and SMTP passwords log in to the provider; they do not
authenticate the person sending a task.

Merge this mail configuration into the [sample project](samples/project).
This example uses one mailbox, `project.agent@example.test`, for the whole
project. Each Agent gets a distinct `+tag` address for routing and replies,
but all Agents send from the untagged mailbox. The top-level `mail.address`
is a template for every Agent; override it with `agents.<id>.mail.address`
as needed. Resolved Agent addresses must be unique.

```yaml
mail:
  address: "project.agent+{{ agent.id }}@example.test"
  trust: ["maintainer@example.test"]
  transport:
    imap:
      endpoint: { host: imap.example.test, port: 993 }
      login:
        username: project.agent@example.test
        password: { file: "/home/agent/.config/agents/imap-password" }
      mailbox: INBOX
    smtp:
      from: project.agent@example.test
      endpoint: { host: smtp.example.test, port: 587, security: starttls }
      login:
        username: project.agent@example.test
        password: { file: "/home/agent/.config/agents/smtp-password" }
    signatures:
      openpgp_keyring: "/home/agent/.config/agents/trusted.gpg"
      trusted:
        - method: openpgp
          address: "maintainer@example.test"
          # Replace with the maintainer's OpenPGP primary-key fingerprint.
          fingerprint: "0000000000000000000000000000000000000000"
        - method: smime
          address: "maintainer@example.test"
          # Replace with the SHA-256 fingerprint of their S/MIME certificate.
          fingerprint: "0000000000000000000000000000000000000000000000000000000000000000"
      signing:
        project.agent@example.test:
          method: smime
          certificate: "/home/agent/.config/agents/signing.pem"
          key: "/home/agent/.config/agents/signing-key.pem"
```

Use the provider's app-specific password where required. To reach the
`reviewer` Agent, send signed mail to `project.agent+reviewer@example.test`
from any mail client that signs with OpenPGP/MIME or S/MIME, such as
Thunderbird or Apple Mail. Every Agent of the project in `To` or `Cc` gets
its own copy; addresses of other projects and people are kept for replies.

`mail.transport.smtp.from` sets both the outgoing `From:` and the SMTP
envelope sender. When it differs from the Agent address, the outgoing
`Reply-To:` is the Agent address so email clients direct replies to its tag.
If `smtp.from` is omitted, the Agent's own address is used for SMTP as before.
The signing key is looked up first by Agent address, then by `smtp.from`.
Outgoing mail is standard `multipart/signed` mail that ordinary clients
verify. The signed part repeats `From`, `To`, `Cc`, `Subject`, `Date`,
`Message-ID`, the thread headers and the Agent's `Reply-To` as
[protected headers](https://datatracker.ietf.org/doc/draft-ietf-lamps-header-protection/).
For signed Agent-to-Agent mail between separate projects, use a distinct
signing identity for each Agent and pin it to that Agent's routing address
on the receiving side. A shared key authenticates the project mailbox, not
individual Agents; `agents` does not trust the outer `Reply-To:` of signed mail.

For received OpenPGP mail, export trusted public keys to the binary keyring using
`gpg --export <fingerprint> > trusted.gpg`. For S/MIME, obtain the leaf
fingerprint with `openssl x509 -in signer.pem -noout -sha256 -fingerprint`.
The exact pinned certificate, not the public CA store or `From`, identifies
the signer. Remove obsolete pins when replacing a certificate or key.

The sender of signed mail is always the pinned signer. When the signed part
carries protected headers, as Thunderbird and `agents` write them, those
headers replace the outer ones; otherwise the outer `To`, `Cc`, `Subject`,
`Date`, `Message-ID` and thread headers are used, and the outer `Reply-To`
is ignored. Supported input is OpenPGP/MIME and detached or opaque S/MIME;
attachments inside the signed part are kept. The importer does not decrypt
encrypted mail.

A signature older than `mail.transport.signatures.max_age` seconds (default 7 days), or
dated in the future, is rejected. Within that window a replayed message is
recognized by its signed `Message-ID`, or by the signed content when it has
none, and imported only once. S/MIME certificates must also be within their
validity period.

```sh
agents mail receive                  # Import unseen IMAP mail without running agents
agents mail deliver < message.eml    # Verify and import a saved message
agents --once                        # Import, flush, and handle the backlog
```

The service keeps one IMAP connection open and waits with IDLE when the
server supports it; otherwise it checks every `mail.transport.imap.poll_interval` seconds
(default 60). Connection failures are retried with exponential backoff up to
five minutes. Every unseen message in the source folder is imported or
rejected and then marked `\Seen`, which serves as the receive cursor. A
rejected message also gets the `AgentsRejected` keyword when the server
allows it, and does not hold up the messages after it. On upgrading, unseen
mail already in the folder is processed once. Accepted mail and replay
records are kept in the project's private local inbox. The outbox is local by default;
an optional `mail.transport.outbox` names a separate private IMAP folder
that supports permanent custom keywords. SMTP deliveries are signed per
recipient with the configured signing identity. For OpenPGP
sending, use `signing.<address>: { method: openpgp, fingerprint: "...",
home: "/path/to/private/gnupg-home" }`. Protect private files and make sure
the service can sign without an interactive prompt. Non-Nix installations
need GnuPG for OpenPGP and the OpenSSL CLI for S/MIME on `PATH`.

### DKIM and ARC through a trusted provider

Mail from ordinary webmail and clients that do not sign can be authenticated
by the DKIM verdict of the receiving mail provider:

```json
"transport": {
  "dkim": {
    "authserv_ids": ["mx.example.com"],
    "arc_sealers": ["google.com"]
  }
}
```

`authserv_ids` lists the provider's identifiers in `Authentication-Results`.
`agents` does not verify DKIM signatures itself; it relies on the provider,
which **must remove `Authentication-Results` headers claiming its
identifiers from incoming mail**. Use this only with such a provider. The
topmost `Authentication-Results` must carry a listed identifier, and no other
header may claim one. Mail is accepted when the provider reports `dkim=pass`
for a signature whose domain is the `From` domain or a parent of it (relaxed
alignment, without the public suffix list), and the sender is in
`mail.trust`. SPF or DMARC passes alone are not enough.

Only headers covered by the signature's `h=` are used; others are dropped.
Signatures with a body length limit (`l=`) are rejected. The signature time
(`t=`, otherwise the signed `Date`) is subject to `signatures.max_age`, and
replays are recognized by the signed `Message-ID` or the signature.

For forwarded mail, `arc_sealers` lists trusted forwarders. When the provider
reports `arc=pass` and the latest ARC set is sealed by a listed domain, the
DKIM verdict recorded by that forwarder is used instead. With DKIM
configured, `agents mail deliver` also accepts such mail. Authenticated mail
is handled the same way however it was authenticated.

### Optional unsigned ingress

`mail.transport.dangerously_allow_unsigned: true` permits unsigned IMAP mail
addressed to local agents in `To` or `Cc`, and enables explicit local import
with `agents mail deliver --unsigned [--recipient <address>]`. Unsigned IMAP
mail that arrives while the switch is off is rejected. Delivering the same
unsigned message again does not import it twice.
Unsigned mail can trigger an agent, receive replies and resume a thread
under the normal mail rules. It is labeled `authenticated: false`.
**Enable this only if you accept unauthenticated sender, reply and thread
headers.** Anyone who can deliver mail to the folder can forge these
headers, including addresses in `mail.trust`. Turning the switch off also
hides previously imported unsigned mail from the agent. Mail that still
presents a signature but fails validation is not downgraded.

When upgrading from 0.1, remove the old gateway integration. Neither the
gateway headers nor `mail deliver --authenticated-from` are accepted as
proof of identity. Archive the old inbox if you need its history; records
in the old gateway format are not imported into the new inbox view.

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
A lost acknowledgment can cause a duplicate, which a receiving `agents`
recognizes by its signed `Message-ID`. Retries are per recipient,
use [Postfix-style backoff](https://www.postfix.org/qmgr.8.html), and expire
after five days by default. A message that cannot be delivered does not hold
up the others. A successful `send` means queued, not delivered. Finished
outbox records are removed after `mail.transport.retry.retain` seconds (default 7 days;
0 keeps them).

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

### MCP servers

Put server entries from an `mcpServers` block directly under `mcp` in the
project config:

```yaml
mcp:
  local-tools:
    command: npx
    args: [-y, example-mcp-server]
    env: { LOG_LEVEL: info }
  remote-tools:
    type: streamable-http
    url: https://mcp.example.test/mcp
    headers:
      Authorization: { env: MCP_AUTHORIZATION }
```

For local servers, `type: stdio` is optional. For remote servers, specify
`type: http` (also accepted as `streamable-http`), `sse` (legacy), or
`ws` (WebSocket). Remote headers may use external string references, as in
the example, where `MCP_AUTHORIZATION` contains the entire header value.

The sample adapters all support stdio and Streamable HTTP. The Claude Code
adapter also supports SSE and WebSocket. The Codex and OpenCode adapters
support an optional stdio `cwd`; the Claude Code adapter rejects it.
Unsupported transports fail during plan rendering, before starting the harness.

### Harness adapters

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
for none, which is how optional arguments are written. An environment variable
script may return `null` to remove the variable; `env` itself may also be a
script that returns the whole map, which the sample Codex adapter uses to keep
HTTP header values out of its arguments. A `json` file script may return any
JSON value.

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
Dovecot and a test SMTP server on loopback, generates test signing keys and
certificates, and uses a fake harness. It needs no Docker, root privileges,
real mail accounts, or harness login.

Source layout, under `crates/agents/src/`:

- `cli/`: argument parsing and stdin/stdout; no logic of its own.
- `config/`: project discovery, layer merging, value references, mail routing,
  and the JSON Schema.
- `harness.rs`: renders an adapter into a plan and launches it.
- `template.rs`: MiniJinja and sandboxed QuickJS evaluation of configured
  values.
- `mail/`: mailboxes (local mbox or IMAP), the outbox and its retries, native
  session bindings, and the per-run `agents-mail` MCP bridge.
- `daemon.rs`: the default command; flushes the outbox and runs each agent on
  its unread mail, continuously or `--once`.
- `service.rs`: user service definitions for systemd and launchd.
- `types.rs`: validated string types shared by all of the above.

Integration tests live in `tests/cli/`; the live mail test is
`tests/mail_e2e.py`.
