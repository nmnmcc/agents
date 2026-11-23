"""Live IMAP/SMTP round trip with an ordinary Dovecot mailbox and real signatures."""

import argparse
import email
import email.policy
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

BUILDER = "agents+build@example.test"
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
    for address in (HUMAN, REVIEWER):
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
            f"/CN=Fixture/emailAddress={address}",
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


def signed(raw, identity, *, opaque=False, detached=False):
    entity = b"Content-Type: text/plain; charset=utf-8\r\n\r\n" + raw
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
    else:
        gpg = (
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
        )
        if detached:
            signature = command(*gpg, "--detach-sign", task=entity)
            mime = (
                b'Content-Type: multipart/signed; protocol="application/pgp-signature"; boundary=pgp\r\n\r\n'
                b"--pgp\r\n" + entity + b"\r\n--pgp\r\n"
                b"Content-Type: application/pgp-signature\r\n\r\n"
                + signature.replace(b"\n", b"\r\n")
                + b"\r\n--pgp--\r\n"
            )
        else:
            mime = b"Content-Type: text/plain; charset=utf-8\r\n\r\n" + command(
                *gpg, "--clearsign", task=raw
            ).replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
    # All transport headers below are intentionally false.
    return (
        b"From: impostor@example.test\r\nTo: wrong@example.test\r\n"
        b"Reply-To: impostor@example.test\r\nSubject: Untrusted subject\r\n"
        b"Message-ID: <outer@example.test>\r\n"
        b"X-Agents-Authenticated-From: review@example.test\r\n" + mime
    )


def job(to, id, body, extra=""):
    return (
        f"Agent-To: {to}\r\nTask-ID: {id}@example.test\r\nSubject: Work\r\n"
        f"{extra}\r\n{body}"
    ).encode()


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
                        "args": [str(harness), "{{ mcp | tojson }}"],
                        "capabilities": {"mcp": ["stdio"]},
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
                    self.wfile.write(b"250 OK\r\n")
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
                job(BUILDER, "pgpmime", "PGP MIME"), keys[HUMAN], detached=True
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
                    b"\r\nX-Agents-Local-From: human@example.test\r\n"
                    b"X-Agents-Envelope-To: agents+build@example.test\r\n"
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
                smime,
                smime.replace(b"Detached S/MIME", b"Tampered S/MIME"),
                unsigned,
            ):
                append(imap_port, "build", item)
            mail(build, "deliver", task=malformed, success=False)
            mail(build, "deliver", "--recipient", REVIEWER, task=raw, success=False)
            mail(
                build,
                "deliver",
                "--unsigned",
                "--recipient",
                BUILDER,
                task=unsigned,
                success=False,
            )
            mail(build, "receive")
            assert not smtp.sent
            items = json.loads(mail(build, "list").stdout)["messages"]
            assert len(items) == 3, items
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
            assert cli(build, "--once").stdout.count(b"handled") == 3
            assert not cli(build, "--once").stdout
            assert not search(build, "Anonymous task")
            mail(build, "deliver", task=replay)
            assert len(search(build, "评审")) == 1

            (followup,) = search(build, "Detached S/MIME")
            mail(build, "reply", *reference(followup), task=b"CLI reply")
            mail(build, "flush")
            assert smtp.sent[-1][:2] == (BUILDER, HUMAN)
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
            assert first[:2] == duplicate[:2] == (BUILDER, REVIEWER)
            assert first[2]["Message-ID"] == duplicate[2]["Message-ID"]
            assert duplicate[2].get_content_type() == "application/pkcs7-mime"
            assert cli(review, "--once").stdout.count(b"handled") == 1
            (reviewed,) = search(review, "Handle on review")
            mail(review, "reply", *reference(reviewed), task=b"Back from review")
            mail(review, "flush")
            assert cli(build, "--once").stdout.count(b"handled") == 1
            (reply,) = search(build, "Back from review")
            assert reply["from"] == REVIEWER

            configuration = json.loads(build.read_text())
            configuration["mail"]["transport"]["allow_unsigned"] = True
            build.write_text(json.dumps(configuration))
            assert cli(build, "--once").stdout.count(b"handled") == 1
            (anonymous,) = search(build, "Anonymous task")
            assert not anonymous["authenticated"] and anonymous["from"] != HUMAN
            error = mail(
                build,
                "reply",
                *reference(anonymous),
                task=b"Do not reply",
                success=False,
            )
            assert b"no trusted reply address" in error.stderr
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
            mail(build, "deliver", "--unsigned", "--recipient", BUILDER, task=unsigned)
            assert cli(build, "--once").stdout.count(b"handled") == 1
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
