"""Live IMAP/SMTP round trip with an ordinary Dovecot mailbox and real signatures."""

import argparse
import email
import email.policy
import email.utils
import grp
import imaplib
import json
import os
import pwd
import re
import resource
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import contextmanager
from pathlib import Path

BUILD_SMTP = "agents@example.test"
BUILDER = "agents+build@example.test"
EDITOR = "agents+edit@example.test"
REVIEWER = "review@example.test"
HUMAN = "human@example.test"


def imap_login(port):
    connection = imaplib.IMAP4("127.0.0.1", port, timeout=5)
    connection.login("test", "pass")
    return connection


def append(port, folder, raw):
    with imap_login(port) as connection:
        assert connection.append(folder, None, None, raw)[0] == "OK"


def command(*args, task=None):
    proc = subprocess.run(args, input=task, capture_output=True, timeout=30)
    assert proc.returncode == 0, (args, proc.stderr.decode(errors="replace"))
    return proc.stdout


def credentials(root):
    home = root / "gpg"
    home.mkdir(mode=0o700)
    gpg = ("gpg", "--batch", "--homedir", home)
    identities = {}
    for address in (HUMAN, REVIEWER, BUILD_SMTP):
        command(
            *gpg,
            "--pinentry-mode",
            "loopback",
            "--passphrase",
            "",
            "--quick-generate-key",
            address,
            "ed25519",
            "sign",
            "0",
        )
        listing = command(*gpg, "--with-colons", "--fingerprint", address)
        fingerprints = [
            line.split(b":")[9].decode()
            for line in listing.splitlines()
            if line.startswith(b"fpr:")
        ]
        assert len(fingerprints) == 1
        identities[address] = {
            "method": "openpgp",
            "fingerprint": fingerprints[0],
            "home": str(home),
        }
    keyring = root / "pubring.gpg"
    keyring.write_bytes(command(*gpg, "--export"))
    for address in (HUMAN, BUILDER):
        key = root / f"{address}.key"
        cert = root / f"{address}.pem"
        command(
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "2",
            "-subj",
            # "+" separates multi-valued RDNs in -subj; escape it in +tag addresses.
            "/CN=Fixture/emailAddress=" + address.replace("+", "\\+"),
            "-keyout",
            key,
            "-out",
            cert,
        )
        fingerprint = (
            command("openssl", "x509", "-in", cert, "-noout", "-fingerprint", "-sha256")
            .decode()
            .strip()
            .split("=", 1)[1]
        )
        identities[f"{address}/smime"] = {
            "method": "smime",
            "fingerprint": fingerprint,
            "certificate": str(cert),
            "key": str(key),
        }
    return keyring, identities


def signed(raw, identity, *, opaque=False):
    """Signs `raw`, a protected-header block and body, as standard OpenPGP/MIME or S/MIME."""
    headers, body = raw.split(b"\r\n\r\n", 1)
    entity = (
        b'Content-Type: text/plain; charset=utf-8; protected-headers="v1"\r\n'
        + headers
        + b"\r\n\r\n"
        + body
    )
    if identity["method"] == "smime":
        options = ["-nodetach", "-binary"] if opaque else []
        mime = command(
            "openssl",
            "cms",
            "-sign",
            *options,
            "-signer",
            identity["certificate"],
            "-inkey",
            identity["key"],
            task=entity,
        )
        # OpenSSL may write LF for S/MIME wrapper headers.
        mime = mime.replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
        # Keep only the MIME wrapper; the outer headers follow below.
        mime = b"Content-Type:" + mime.split(b"Content-Type:", 1)[1]
    else:
        signature = command(
            "gpg",
            "--batch",
            "--homedir",
            identity["home"],
            "--local-user",
            identity["fingerprint"],
            "--digest-algo",
            "SHA256",
            "--armor",
            "--output",
            "-",
            "--detach-sign",
            task=entity,
        )
        mime = (
            b"Content-Type: multipart/signed; micalg=pgp-sha256;\r\n"
            b' protocol="application/pgp-signature"; boundary=pgp\r\n\r\n'
            b"--pgp\r\n" + entity + b"\r\n--pgp\r\n"
            b"Content-Type: application/pgp-signature\r\n\r\n"
            + signature.replace(b"\n", b"\r\n")
            + b"\r\n--pgp--\r\n"
        )
    # Every outer header is intentionally false: only the signed headers count.
    return (
        b"From: impostor@example.test\r\nTo: wrong@example.test\r\n"
        b"Reply-To: impostor@example.test\r\nSubject: Untrusted subject\r\n"
        b"Message-ID: <outer@example.test>\r\nMIME-Version: 1.0\r\n"
        b"X-Agents-Authenticated-From: review@example.test\r\n" + mime
    )


def job(to, id, body, extra=""):
    """An ordinary message from a mail client; `signed` protects its headers."""
    return (
        f"From: {HUMAN}\r\nTo: {to}\r\nMessage-ID: <{id}@example.test>\r\n"
        f"Subject: Work\r\n{extra}\r\n{body}"
    ).encode()


def signed_parts(raw):
    """Splits a multipart/signed message into its exact signed entity and signature."""
    message = email.message_from_bytes(raw, policy=email.policy.default)
    assert message.get_content_type() == "multipart/signed", message
    delimiter = b"\r\n--" + message.get_boundary().encode()
    body = raw.split(b"\r\n\r\n", 1)[1]
    _, entity, signature = (b"\r\n" + body).split(delimiter)[:3]
    signature = email.message_from_bytes(signature.split(b"\r\n", 1)[1])
    return entity.split(b"\r\n", 1)[1], signature.get_payload(decode=True)


def verify_pgp(raw, home, directory):
    entity, signature = signed_parts(raw)
    (directory / "entity").write_bytes(entity)
    (directory / "entity.asc").write_bytes(signature)
    command(
        "gpg",
        "--batch",
        "--homedir",
        home,
        "--verify",
        directory / "entity.asc",
        directory / "entity",
    )
    return entity


def project(root, role, address, port, smtp, keyring, trusted, signing):
    root.mkdir()
    harness = Path(__file__).resolve().with_name("fixtures") / "fake_mail_harness.py"
    path = root / ".agents.json"
    path.write_text(
        json.dumps(
            {
                "version": 1,
                "agent": role,
                "agents": {
                    role: {"harness": "fake", "description": role, "instructions": role}
                },
                "harnesses": {
                    "fake": {
                        "program": sys.executable,
                        "args": [
                            str(harness),
                            "{{ mcp | tojson }}",
                            "{{ session | tojson }}",
                        ],
                        "capabilities": {"mcp": ["stdio"], "sessions": "supplied"},
                    },
                },
                "mail": {
                    "address": address,
                    "trust": [HUMAN, BUILDER, REVIEWER],
                    "transport": {
                        "imap": {
                            "endpoint": {
                                "host": "127.0.0.1",
                                "port": port,
                                "security": "loopback",
                            },
                            "login": {
                                "username": "test",
                                "password": {"env": "AGENTS_SMOKE_IMAP"},
                            },
                            "mailbox": role,
                        },
                        "smtp": {
                            "endpoint": {
                                "host": "127.0.0.1",
                                "port": smtp,
                                "security": "loopback",
                            },
                        },
                        "retry": {"minimal_backoff_time": 2, "maximal_backoff_time": 2},
                        "signatures": {
                            "openpgp_keyring": str(keyring),
                            "trusted": [
                                {
                                    "address": address,
                                    **{
                                        key: identity[key]
                                        for key in ("method", "fingerprint")
                                    },
                                }
                                for address, identity in trusted
                            ],
                            "signing": {
                                address: {
                                    key: val
                                    for key, val in signing.items()
                                    if key != "fingerprint"
                                    or signing["method"] == "openpgp"
                                },
                            },
                        },
                    },
                },
            }
        )
    )
    return path


class Smtp(socketserver.StreamRequestHandler):
    def handle(self):
        self.wfile.write(b"220 loopback ESMTP\r\n")
        sender = recipient = None
        for line in self.rfile:
            match line.split(maxsplit=1)[0].upper():
                case b"EHLO" | b"HELO":
                    self.wfile.write(b"250 loopback\r\n")
                case b"MAIL":
                    sender = re.search(rb"<([^<>]+)>", line)[1].decode()
                    if sender in (BUILD_SMTP, REVIEWER):
                        self.wfile.write(b"250 OK\r\n")
                    else:
                        self.wfile.write(b"553 sender is not authorized\r\n")
                case b"RCPT":
                    recipient = re.search(rb"<([^<>]+)>", line)[1].decode()
                    self.wfile.write(b"250 OK\r\n")
                case b"DATA":
                    self.wfile.write(b"354 end with dot\r\n")
                    parts = []
                    while (row := self.rfile.readline()) != b".\r\n":
                        if not row:
                            return
                        parts.append(row[1:] if row.startswith(b"..") else row)
                    raw = b"".join(parts)
                    self.server.raw.append(raw)
                    self.server.sent.append(
                        (
                            sender,
                            recipient,
                            email.message_from_bytes(raw, policy=email.policy.default),
                        )
                    )
                    if folder := {BUILDER: "build", REVIEWER: "review"}.get(recipient):
                        append(self.server.imap_port, folder, raw)
                    if self.server.lose_ack:
                        self.server.lose_ack = False
                        return
                    self.wfile.write(b"250 accepted\r\n")
                case b"QUIT":
                    self.wfile.write(b"221 bye\r\n")
                    return
                case _:
                    self.wfile.write(b"500 unexpected command\r\n")


def test(binary, imap_port):
    with (
        tempfile.TemporaryDirectory(prefix="agents-") as directory,
        socketserver.TCPServer(("127.0.0.1", 0), Smtp) as smtp,
    ):
        smtp.imap_port = imap_port
        smtp.sent = []
        smtp.raw = []
        smtp.lose_ack = False
        root = Path(directory)
        keyring, keys = credentials(root)
        trusted = [(HUMAN, keys[HUMAN]), (HUMAN, keys[f"{HUMAN}/smime"])]
        port = smtp.server_address[1]
        build = project(
            root / "builder",
            "build",
            BUILDER,
            imap_port,
            port,
            keyring,
            [*trusted, (REVIEWER, keys[REVIEWER])],
            keys[f"{BUILDER}/smime"],
        )
        config = json.loads(build.read_text())
        config["agents"]["edit"] = {
            **config["agents"]["build"],
            "mail": {"address": EDITOR},
        }
        config["mail"]["transport"]["smtp"]["from"] = BUILD_SMTP
        config["mail"]["transport"]["signatures"]["signing"][BUILD_SMTP] = keys[
            BUILD_SMTP
        ]
        build.write_text(json.dumps(config))
        review = project(
            root / "reviewer",
            "review",
            REVIEWER,
            imap_port,
            port,
            keyring,
            [(BUILDER, keys[f"{BUILDER}/smime"])],
            keys[REVIEWER],
        )
        with imap_login(imap_port) as connection:
            for folder in ("build", "review"):
                assert connection.create(folder)[0] == "OK"
        thread = threading.Thread(target=smtp.serve_forever, daemon=True)
        thread.start()
        try:

            def cli(config, *args, task=None, success=True):
                proc = subprocess.run(
                    [binary, *args],
                    cwd=config.parent,
                    capture_output=True,
                    input=task,
                    env=dict(os.environ, AGENTS_SMOKE_IMAP="pass"),
                    timeout=30,
                )
                assert (proc.returncode == 0) == success, (args, proc.stderr.decode())
                return proc

            def mail(config, *args, **kwargs):
                return cli(config, "mail", *args, **kwargs)

            def search(config, term):
                return json.loads(mail(config, "search", term).stdout)["messages"]

            def reference(summary):
                ref = summary["mail"]
                return "--uid-validity", str(ref["uid_validity"]), str(ref["uid"])

            raw = signed(job(BUILDER, "start", "PGP text 评审"), keys[HUMAN])
            malformed = raw.replace(b"PGP text", b"PGP evil")
            replay = raw.replace(b"<outer@example.test>", b"<replay@example.test>")
            other = signed(job(REVIEWER, "other", "wrong target"), keys[HUMAN])
            untrusted = signed(
                job(BUILDER, "wrong-key", "untrusted certificate"),
                keys[f"{BUILDER}/smime"],
            )
            pgp_mime = signed(
                job(BUILDER, "pgpmime", "PGP MIME", f"Cc: {REVIEWER}\r\n"),
                keys[HUMAN],
            )
            opaque = signed(
                job(BUILDER, "opaque", "Opaque S/MIME"),
                keys[f"{HUMAN}/smime"],
                opaque=True,
            )
            smime = signed(
                job(
                    BUILDER,
                    "smime",
                    "Detached S/MIME",
                    "In-Reply-To: <start@example.test>\r\nReferences: <start@example.test>\r\n",
                ),
                keys[f"{HUMAN}/smime"],
            )
            unsigned = (
                b"To: "
                + BUILDER.encode()
                + b"\r\nFrom: "
                + HUMAN.encode()
                + (
                    b"\r\nX-Agents-Local-From: impostor@example.test\r\n"
                    b"X-Agents-Envelope-To: agents+build@example.test\r\n"
                    b"Message-ID: <unsigned@example.test>\r\n"
                    b"In-Reply-To: <start@example.test>\r\n"
                    b"References: <start@example.test>\r\n"
                    b"Subject: Unsigned\r\n\r\nAnonymous task"
                )
            )
            for item in (
                raw,
                malformed,
                replay,
                other,
                untrusted,
                pgp_mime,
                opaque,
                smime,
                smime.replace(b"Detached S/MIME", b"Tampered S/MIME"),
                unsigned,
            ):
                append(imap_port, "build", item)
            mail(build, "deliver", task=malformed, success=False)
            mail(build, "deliver", "--recipient", REVIEWER, task=raw, success=False)
            mail(build, "deliver", "--unsigned", task=unsigned, success=False)
            mail(build, "receive")
            assert not smtp.sent
            with imap_login(imap_port) as connection:
                connection.select("build")
                # Every source message was consumed; a bad one does not hold up the rest.
                assert connection.search(None, "UNSEEN")[1] == [b""]
                rejected = connection.search(None, "KEYWORD", "AgentsRejected")[1]
                # malformed, other, untrusted, tampered and unsigned mail
                assert len(rejected[0].split()) == 5, rejected
            items = json.loads(mail(build, "list").stdout)["messages"]
            assert len(items) == 4, items
            assert all(
                entry["from"] == HUMAN
                and entry["authenticated"]
                and entry["subject"] == "Work"
                for entry in items
            )
            (initial,) = search(build, "评审")
            thread_items = json.loads(
                mail(build, "thread", *reference(initial)).stdout
            )["messages"]
            assert len(thread_items) == 2, thread_items
            handled = cli(build, "--once").stdout.splitlines()
            assert len(handled) == 4
            initial_session = handled[0].split()[1]
            assert not cli(build, "--once").stdout
            assert not search(build, "Anonymous task")
            mail(build, "deliver", task=replay)
            assert len(search(build, "评审")) == 1

            (followup,) = search(build, "Detached S/MIME")
            mail(build, "reply", *reference(followup), task=b"CLI reply")
            mail(build, "flush")
            assert smtp.sent[-1][:2] == (BUILD_SMTP, HUMAN)
            assert smtp.sent[-1][2]["From"] == BUILD_SMTP
            assert smtp.sent[-1][2]["Reply-To"] == BUILDER
            assert smtp.sent[-1][2]["In-Reply-To"] == "<smime@example.test>"

            mail(
                build,
                "send",
                "--to",
                REVIEWER,
                "--subject",
                "Cross host",
                task=b"Handle on review",
            )
            smtp.lose_ack = True
            mail(build, "flush", success=False)
            assert len(json.loads(mail(build, "pending").stdout)["queued"]) == 1
            time.sleep(2.2)
            mail(build, "flush")
            first, duplicate = smtp.sent[-2:]
            assert first[:2] == duplicate[:2] == (BUILD_SMTP, REVIEWER)
            assert first[2]["Message-ID"] == duplicate[2]["Message-ID"]
            assert duplicate[2].get_content_type() == "multipart/signed"
            assert cli(review, "--once").stdout.count(b"handled") == 1
            (reviewed,) = search(review, "Handle on review")
            mail(review, "reply", *reference(reviewed), task=b"Back from review")
            mail(review, "flush")
            assert smtp.sent[-1][:2] == (REVIEWER, BUILDER)
            assert smtp.sent[-1][2]["From"] == REVIEWER
            assert smtp.sent[-1][2]["Reply-To"] is None
            assert cli(build, "--once").stdout.count(b"handled") == 1
            (reply,) = search(build, "Back from review")
            assert reply["from"] == REVIEWER

            # Signed Cc headers survive import, so replies can include everyone.
            (copied,) = search(build, "PGP MIME")
            mail(build, "reply", "--all", *reference(copied), task=b"To all")
            mail(build, "flush")
            assert {sent[1] for sent in smtp.sent[-2:]} == {HUMAN, REVIEWER}
            assert all(
                sent[2]["In-Reply-To"] == "<pgpmime@example.test>"
                for sent in smtp.sent[-2:]
            )

            configuration = json.loads(build.read_text())
            configuration["mail"]["transport"]["dangerously_allow_unsigned"] = True
            build.write_text(json.dumps(configuration))
            # Unsigned IMAP mail was rejected while disabled; deliver it again.
            mail(build, "deliver", "--unsigned", task=unsigned)
            assert (
                cli(build, "--once").stdout.strip()
                == b"handled " + initial_session + b" resume"
            )
            (anonymous,) = search(build, "Anonymous task")
            assert not anonymous["authenticated"] and anonymous["from"] == HUMAN
            mail(build, "reply", *reference(anonymous), task=b"Reply to unsigned task")
            mail(build, "flush")
            assert smtp.sent[-1][:2] == (BUILD_SMTP, HUMAN)
            assert smtp.sent[-1][2]["In-Reply-To"] == "<unsigned@example.test>"
            assert not cli(build, "--once").stdout
            mail(
                build,
                "deliver",
                "--unsigned",
                "--recipient",
                BUILDER,
                task=malformed,
                success=False,
            )
            # The same unsigned message is imported once.
            mail(build, "deliver", "--unsigned", "--recipient", BUILDER, task=unsigned)
            assert not cli(build, "--once").stdout
            configuration["mail"]["transport"]["dangerously_allow_unsigned"] = False
            build.write_text(json.dumps(configuration))
            assert not search(build, "Anonymous task")
            assert not cli(build, "--once").stdout

            # A trusted provider's DKIM verdict authenticates ordinary mail.
            configuration["mail"]["transport"]["dkim"] = {
                "authserv_ids": ["mx.provider.test"]
            }
            build.write_text(json.dumps(configuration))

            def provider(id, body, verdicts):
                stamp = int(time.time())
                return (
                    "".join(
                        f"Authentication-Results: {verdict}\r\n" for verdict in verdicts
                    )
                    + "DKIM-Signature: v=1; a=rsa-sha256; d=example.test; s=mail;"
                    f" h=from:to:subject:date:message-id; t={stamp}; bh=AAAA;"
                    " b=DkimProof1234567890\r\n"
                    f"Date: {email.utils.formatdate()}\r\n"
                ).encode() + job(BUILDER, id, body)

            passed = (
                "mx.provider.test; dkim=pass header.d=example.test header.b=DkimProof1"
            )
            forged = [
                # A verdict from a server the provider does not vouch for.
                provider(
                    "dkim-forged", "Forged verdict", ["mx.evil.test; " + passed[18:]]
                ),
                # A sender-supplied verdict below the provider's own failing one.
                provider(
                    "dkim-buried",
                    "Buried verdict",
                    ["mx.provider.test; dkim=fail header.d=example.test", passed],
                ),
            ]
            for message in forged:
                append(imap_port, "build", message)
            append(imap_port, "build", provider("dkim", "DKIM task", [passed]))
            assert cli(build, "--once").stdout.count(b"handled") == 1
            (dkim,) = search(build, "DKIM task")
            assert dkim["authenticated"] and dkim["from"] == HUMAN
            assert not search(build, "Forged verdict")
            assert not search(build, "Buried verdict")
            with imap_login(imap_port) as connection:
                connection.select("build")
                rejected = connection.search(None, "KEYWORD", "AgentsRejected")[1]
                assert len(rejected[0].split()) == 7, rejected
            # Without the provider's verdict, the same mail cannot be delivered.
            mail(
                build,
                "deliver",
                task=provider("dkim-local", "Local", ["mx.evil.test; " + passed[18:]]),
                success=False,
            )

            # Standard mail to a +tag address reaches that agent.
            editor_task = signed(
                job(EDITOR, "edit", "Please acknowledge this task"), keys[HUMAN]
            )
            append(imap_port, "build", editor_task)
            started = cli(build, "--once").stdout.strip().split()
            assert started[0] == b"handled" and started[2] == b"start"
            assert not search(build, "Please acknowledge this task")
            mail(build, "flush")
            sender, recipient, outbound = smtp.sent[-1]
            assert (sender, recipient) == (BUILD_SMTP, HUMAN)
            assert outbound["From"] == BUILD_SMTP
            assert outbound["Reply-To"] == EDITOR
            entity = verify_pgp(smtp.raw[-1], keys[BUILD_SMTP]["home"], root)
            assert b'protected-headers="v1"' in entity
            assert f"To: {HUMAN}".encode() in entity
            assert f"Reply-To: {EDITOR}".encode() in entity
            assert b"Task acknowledged" in entity

            parent = outbound["Message-ID"]
            follow_up = signed(
                job(
                    EDITOR,
                    "edit-follow-up",
                    "Thanks for your reply",
                    f"In-Reply-To: {parent}\r\n",
                ),
                keys[HUMAN],
            )
            append(imap_port, "build", follow_up)
            # Each agent may render its own prompt from the message.
            configuration = json.loads(build.read_text())
            configuration["agents"]["edit"]["mail"]["prompt"] = (
                "Custom prompt for {{ agent.id }} at {{ address }}: "
                "{{ mail.message.subject }}\n{{ mail | json }}\n"
            )
            build.write_text(json.dumps(configuration))
            assert cli(build, "--once").stdout.strip().splitlines() == [
                f"prompt: Custom prompt for edit at {EDITOR}: Work".encode(),
                b"handled " + started[1] + b" resume",
            ]

            sent_count = len(smtp.sent)
            mail(
                build, "send", "--to", "edit", "--subject", "Local", task=b"Local task"
            )
            mail(build, "flush")
            assert len(smtp.sent) == sent_count
            (local,) = json.loads(
                mail(build, "search", "--agent", "edit", "Local task").stdout
            )["messages"]
            assert local["from"] == BUILDER

            daemon_log = root / "daemon.log"
            with daemon_log.open("wb") as log:
                daemon = subprocess.Popen(
                    [binary],
                    cwd=review.parent,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    env=dict(os.environ, AGENTS_SMOKE_IMAP="pass"),
                )
            try:

                def eventually(check, seconds):
                    deadline = time.monotonic() + seconds
                    while time.monotonic() < deadline:
                        if check():
                            return True
                        assert daemon.poll() is None, daemon_log.read_text()
                        time.sleep(0.1)
                    raise AssertionError(daemon_log.read_text())

                def handled(text):
                    found = search(review, text)
                    return len(found) == 1 and not found[0]["unread"]

                # The reply-all copy is already waiting; then the daemon idles.
                eventually(lambda: handled("To all"), 15)
                started = time.monotonic()
                append(
                    imap_port,
                    "review",
                    signed(
                        job(REVIEWER, "idle", "Pushed over IDLE").replace(
                            HUMAN.encode(), BUILDER.encode(), 1
                        ),
                        keys[f"{BUILDER}/smime"],
                    ),
                )
                eventually(lambda: handled("Pushed over IDLE"), 10)
                # Far below the 60 second poll interval: IMAP IDLE woke the receiver.
                assert time.monotonic() - started < 5

                configuration = json.loads(review.read_text())
                configuration["agents"]["review"]["description"] = "Reloaded"
                review.write_text(json.dumps(configuration))
                eventually(
                    lambda: b"configuration reloaded" in daemon_log.read_bytes(), 5
                )
            finally:
                daemon.terminate()
                daemon.wait(timeout=5)
        finally:
            smtp.shutdown()
            thread.join()


def build_binary():
    result = subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "-p",
            "agents",
            "--bin",
            "agents",
            "--message-format=json-render-diagnostics",
        ],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        cwd=Path(__file__).resolve().parents[3],
    )
    for line in result.stdout.splitlines():
        artifact = json.loads(line)
        if (
            artifact["reason"] == "compiler-artifact"
            and artifact["target"]["name"] == "agents"
            and artifact["executable"]
        ):
            return Path(artifact["executable"])
    raise RuntimeError("Cargo did not report the agents executable")


@contextmanager
def dovecot():
    uid, gid = os.geteuid(), os.getegid()
    user = pwd.getpwuid(uid).pw_name
    group = grp.getgrgid(gid).gr_name
    with (
        tempfile.TemporaryDirectory(prefix="imap-") as directory,
        tempfile.TemporaryFile() as log,
    ):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        config = Path(directory) / "dovecot.conf"
        # Dovecot's "unlimited" is not Darwin's RLIM_INFINITY, and macOS
        # rejects finite RLIMIT_DATA limits on Apple Silicon.
        limits = (
            f"default_vsz_limit = {resource.RLIM_INFINITY} B"
            if sys.platform == "darwin"
            else ""
        )
        config.write_text(
            f"""
dovecot_config_version = 2.4.0
dovecot_storage_version = 2.4.0
protocols = imap
listen = 127.0.0.1
ssl = no
auth_allow_cleartext = yes
log_path = /dev/stderr
{limits}
base_dir = "{directory}/run"
state_dir = "{directory}/state"
mail_driver = maildir
mail_path = "{directory}/Maildir"
mail_uid = {uid}
mail_gid = {gid}
first_valid_uid = {uid}
last_valid_uid = {uid}
first_valid_gid = {gid}
default_internal_user = {user}
default_internal_group = {group}
default_login_user = {user}
passdb static {{
  password = pass
}}
userdb static {{
  fields {{
    home = "{directory}"
  }}
}}
service imap-login {{
  chroot =
  inet_listener imap {{
    port = {port}
  }}
}}
""".lstrip()
        )
        process = subprocess.Popen(
            ["dovecot", "-F", "-c", config], stdout=log, stderr=log
        )
        try:
            deadline = time.monotonic() + 10
            while process.poll() is None and time.monotonic() < deadline:
                try:
                    with imap_login(port):
                        break
                except (OSError, imaplib.IMAP4.error):
                    time.sleep(0.1)
            else:
                log.seek(0)
                raise RuntimeError(f"Dovecot failed to start:\n{log.read().decode()}")
            yield port
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "binary", type=Path, nargs="?", help="use this binary instead of building"
    )
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.exit(1, "test-e2e: run as an unprivileged user\n")
    binary = args.binary.resolve(strict=True) if args.binary else build_binary()
    with dovecot() as port:
        test(binary, port)
    print("signed mail round trip passed")


if __name__ == "__main__":
    main()
