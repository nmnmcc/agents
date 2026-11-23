"""Live IMAP/SMTP round trip with an unprivileged Dovecot instance."""

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

MAILBOXES = {
    "build": "agents+build@example.test",
    "review": "review@example.test",
    "qa": "agents+qa@example.test",
    "audit": "agents+audit@example.test",
    "ops": "agents+ops@example.test",
}
TARGETS = [address for role, address in MAILBOXES.items() if role != "build"]
REPLY_TO = "tickets@example.test"


def imap_login(port):
    connection = imaplib.IMAP4("127.0.0.1", port, timeout=5)
    connection.login("test", "pass")
    return connection


def append(port, raw):
    with imap_login(port) as connection:
        assert connection.append("AgentTasks", None, None, raw)[0] == "OK"


def fixture(root, imap_port, smtp_port):
    project_dir = root / "website"
    drops = project_dir / ".agents.d"
    drops.mkdir(parents=True)
    (drops / "10-project.json").write_text(
        json.dumps(
            {
                "version": 1,
                "agent": "build",
                "agents": {
                    role: {"harness": "fake", "description": role, "instructions": role}
                    for role in MAILBOXES
                },
            }
        )
    )
    (drops / "20-mail.json").write_text(
        json.dumps(
            {
                "agents": {
                    "review": {
                        "mail": {
                            "address": "{{ agent.id }}@example.test",
                            "from": ["build"],
                        }
                    },
                },
            }
        )
    )
    harness = Path(__file__).resolve().with_name("fixtures") / "fake_mail_harness.py"
    (drops / "30-transport.json").write_text(
        json.dumps(
            {
                "harnesses": {
                    "fake": {
                        "program": sys.executable,
                        "args": [str(harness), "{{ agent.id }}", "{{ mcp | tojson }}"],
                        "capabilities": {"mcp": ["stdio"]},
                    }
                },
                "mail": {
                    "address": "agents+{{ agent.id }}@example.test",
                    "trust": ["human@example.test", REPLY_TO],
                    "transport": {
                        "imap": {
                            "endpoint": {
                                "host": "127.0.0.1",
                                "port": imap_port,
                                "security": "loopback",
                            },
                            "login": {
                                "username": "test",
                                "password": {"env": "AGENTS_SMOKE_IMAP"},
                            },
                            "mailbox": "AgentTasks",
                        },
                        "outbox": "AgentOutbox",
                        "smtp": {
                            "endpoint": {
                                "host": "127.0.0.1",
                                "port": smtp_port,
                                "security": "loopback",
                            }
                        },
                        "retry": {"minimal_backoff_time": 3, "maximal_backoff_time": 3},
                    },
                },
            }
        )
    )
    return project_dir


class Smtp(socketserver.StreamRequestHandler):
    def handle(self):
        self.wfile.write(b"220 loopback ESMTP\r\n")
        sender = recipient = None
        for line in self.rfile:
            command = line.split(maxsplit=1)[0].upper()
            match command:
                case b"EHLO" | b"HELO":
                    self.wfile.write(b"250 loopback\r\n")
                case b"MAIL" | b"RCPT":
                    mailbox = re.search(rb"<([^<>]+)>", line)[1]
                    if command == b"MAIL":
                        sender = mailbox
                    else:
                        recipient = mailbox
                    self.wfile.write(b"250 OK\r\n")
                case b"DATA":
                    self.wfile.write(b"354 end with dot\r\n")
                    parts = []
                    while (row := self.rfile.readline()) != b".\r\n":
                        if not row:
                            return
                        parts.append(row[1:] if row.startswith(b"..") else row)
                    data = b"".join(parts)
                    self.server.sent.append(
                        (
                            sender,
                            recipient,
                            email.message_from_bytes(data, policy=email.policy.default),
                        )
                    )
                    if recipient.decode("ascii") in MAILBOXES.values():
                        # The fixture acts as the trusted delivery gateway.
                        append(
                            self.server.imap_port,
                            b"X-Agents-Envelope-To: " + recipient + b"\r\n"
                            b"X-Agents-Authenticated-From: " + sender + b"\r\n" + data,
                        )
                    if self.server.lose_ack:
                        self.server.lose_ack = False
                        return
                    self.wfile.write(b"250 accepted\r\n")
                case b"QUIT":
                    self.wfile.write(b"221 bye\r\n")
                    return
                case _:
                    self.wfile.write(b"500 unexpected command\r\n")


def message(sender, role, message_id):
    return (
        f"X-Agents-Envelope-To: {MAILBOXES[role]}\r\n"
        f"X-Agents-Authenticated-From: {sender}\r\n"
        f"From: {sender}\r\n"
        f"Message-ID: <{message_id}@example.test>\r\n"
        "Subject: Work\r\n\r\nDo the work."
    ).encode()


def test(binary, imap_port):
    with (
        tempfile.TemporaryDirectory() as directory,
        socketserver.TCPServer(("127.0.0.1", 0), Smtp) as smtp,
    ):
        smtp.imap_port = imap_port
        smtp.sent = []
        smtp.lose_ack = False
        project = fixture(Path(directory), imap_port, smtp.server_address[1])
        with imap_login(imap_port) as connection:
            for folder in ("AgentTasks", "AgentOutbox"):
                assert connection.create(folder)[0] == "OK"
        thread = threading.Thread(target=smtp.serve_forever, daemon=True)
        thread.start()
        try:

            def cli(*args, success=True, task=None):
                proc = subprocess.run(
                    [binary, *args],
                    cwd=project,
                    text=True,
                    capture_output=True,
                    input=task,
                    env=dict(os.environ, AGENTS_SMOKE_IMAP="pass"),
                    timeout=15,
                )
                assert (proc.returncode == 0) == success, proc.stderr
                return proc

            initial = message("human@example.test", "build", "initial").split(
                b"\r\n\r\n"
            )[0]
            initial += (
                "\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=parts\r\n\r\n"
                "--parts\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
                "A fulltext-payload for 评审.\r\n"
                "--parts\r\nContent-Type: application/octet-stream\r\n"
                "Content-Disposition: attachment; filename=notes.txt\r\n"
                "Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--parts--\r\n"
            ).encode()
            append(imap_port, initial)
            append(
                imap_port,
                message("human@example.test", "build", "followup").replace(
                    b"\r\n\r\n",
                    (
                        f"\r\nReferences: <initial@example.test>\r\nReply-To: {REPLY_TO}\r\n"
                        f"To: {MAILBOXES['build']}, {MAILBOXES['review']}\r\n"
                        f"Cc: {MAILBOXES['review']}, {MAILBOXES['qa']}\r\n"
                        f"Bcc: {MAILBOXES['audit']}\r\n\r\n"
                    ).encode(),
                ),
            )
            append(
                imap_port,
                message("human@example.test", "review", "private").replace(
                    b"Do the work.", b"private-reviewer"
                ),
            )
            cli("mail", "flush")
            assert not smtp.sent, "new inbound mail must not start an agent"
            assert cli("run", project, task="Inspect inbox.").stdout.strip() == "built"
            assert not smtp.sent, "send_mail queues mail for flush"
            cli("mail", "flush")
            assert [item[:2] for item in smtp.sent] == [
                (MAILBOXES["build"].encode(), MAILBOXES["review"].encode())
            ]
            assert (
                cli(
                    "run", "--agent", "review", project, task="Check your inbox."
                ).stdout.strip()
                == "reviewed"
            )
            cli("mail", "flush")
            assert len(smtp.sent) == 1, "a local run must not automatically reply"

            cli("run", project, task="Retry task.")
            smtp.lose_ack = True
            cli("mail", "flush", success=False)
            pending = json.loads(cli("mail", "pending").stdout)
            assert len(pending["queued"]) == 1 and not pending["rejected"], pending
            with imap_login(imap_port) as connection:
                connection.select("AgentOutbox", readonly=True)
                entries = connection.uid(
                    "FETCH",
                    str(pending["queued"][0]),
                    "(BODY.PEEK[HEADER.FIELDS (X-Agents-Queue)])",
                )[1]
                due = int(
                    next(
                        item[1] for item in entries if isinstance(item, tuple)
                    ).split()[2]
                )
            if time.time() < due - 0.5:
                cli("mail", "flush")
                assert len(smtp.sent) == 2, "retry must wait until due"
            time.sleep(max(0, due - time.time() + 0.2))
            cli("mail", "flush")
            assert len(smtp.sent) == 3, "lost 250 must be retried"
            assert smtp.sent[1][2]["Message-ID"] == smtp.sent[2][2]["Message-ID"]
            assert smtp.sent[2][2]["X-Agents-Queue"] is None
            cli("mail", "flush")
            assert not json.loads(cli("mail", "pending").stdout)["queued"]
            assert len(smtp.sent) == 3

            def mail_cli(command, *args, **kwargs):
                return cli("mail", command, "--directory", project, *args, **kwargs)

            first = json.loads(mail_cli("list", "--limit", "1").stdout)
            second = json.loads(
                mail_cli(
                    "list",
                    "--limit",
                    "1",
                    "--before-uid",
                    str(first["next_before_uid"]),
                ).stdout
            )
            assert first["messages"][0]["mail"] != second["messages"][0]["mail"]
            ref = json.loads(mail_cli("search", "fulltext-payload").stdout)["messages"][
                0
            ]["mail"]
            args = ("--uid-validity", str(ref["uid_validity"]), str(ref["uid"]))
            assert "评审" in json.loads(mail_cli("get", *args).stdout)["body"]
            assert len(json.loads(mail_cli("thread", *args).stdout)["messages"]) == 2
            assert (
                json.loads(mail_cli("attachment", *args, "0").stdout)["content_base64"]
                == "aGVsbG8="
            )
            mail_cli("get", "--agent", "review", *args, success=False)
            mail_cli("set-read", *args, "unread")
            assert json.loads(
                mail_cli("search", "fulltext-payload", "--unread-only").stdout
            )["messages"]

            mail_cli(
                "send",
                "--subject",
                "Denied",
                "--to",
                "review",
                "--cc",
                "unapproved",
                task="No",
                success=False,
            )
            assert not json.loads(cli("mail", "pending").stdout)["queued"]
            assert (
                json.loads(
                    mail_cli(
                        "send",
                        "--subject",
                        "Group",
                        "--to",
                        "review",
                        "--to",
                        "qa",
                        "--cc",
                        "ops",
                        "--bcc",
                        "audit",
                        task="Group mail",
                    ).stdout
                )["recipients"]
                == 4
            )
            smtp.lose_ack = True
            cli("mail", "flush", success=False)
            group = smtp.sent[3:]
            assert {entry[1].decode() for entry in group} == set(TARGETS)
            assert len(group) == 4
            queued = json.loads(cli("mail", "pending").stdout)["queued"]
            assert len(queued) == 1
            with imap_login(imap_port) as connection:
                connection.select("AgentOutbox", readonly=True)
                entries = connection.uid(
                    "FETCH",
                    str(queued[0]),
                    "(BODY.PEEK[HEADER.FIELDS (X-Agents-Queue)])",
                )[1]
                due = int(
                    next(
                        item[1] for item in entries if isinstance(item, tuple)
                    ).split()[2]
                )
            time.sleep(max(0, due - time.time() + 0.2))
            cli("mail", "flush")
            assert len(smtp.sent) == 8
            assert smtp.sent[7][1] == group[0][1]
            assert smtp.sent[7][2].as_bytes() == group[0][2].as_bytes()
            assert len({entry[2]["Message-ID"] for entry in group}) == 1
            for _, _, email_msg in group:
                assert [a.addr_spec for a in email_msg["To"].addresses] == [
                    MAILBOXES["review"],
                    MAILBOXES["qa"],
                ]
                assert email_msg["Cc"].addresses[0].addr_spec == MAILBOXES["ops"]
                assert email_msg["Bcc"] is None and email_msg["In-Reply-To"] is None
                assert MAILBOXES["audit"] not in email_msg.as_string()

            follow = json.loads(mail_cli("search", "followup@example.test").stdout)[
                "messages"
            ][0]["mail"]
            mail_cli(
                "reply",
                "--uid-validity",
                str(follow["uid_validity"]),
                str(follow["uid"]),
                task="CLI reply",
            )
            cli("run", project, task="MCP reply.")
            cli("mail", "flush")
            cli_reply, mcp_reply, blind_copy = smtp.sent[8:]
            assert (
                cli_reply[1] == REPLY_TO.encode()
                and mcp_reply[1] == b"human@example.test"
            )
            assert cli_reply[2]["Cc"] is None
            assert cli_reply[2]["Subject"] == "Re: Work"
            assert cli_reply[2]["In-Reply-To"] == "<followup@example.test>"
            assert (
                cli_reply[2]["References"]
                == "<initial@example.test> <followup@example.test>"
            )
            assert mcp_reply[2]["In-Reply-To"] == "<initial@example.test>"
            assert mcp_reply[2]["References"] == "<initial@example.test>"
            assert mcp_reply[2]["Message-ID"] == blind_copy[2]["Message-ID"]
            assert blind_copy[1].decode() == MAILBOXES["audit"]
            assert MAILBOXES["audit"] not in blind_copy[2].as_string()

            mail_cli(
                "send",
                "--subject",
                "Private",
                "--bcc",
                "audit",
                task="Hidden recipient",
            )
            cli("mail", "flush")
            assert smtp.sent[-1][1].decode() == MAILBOXES["audit"]
            assert MAILBOXES["audit"] not in smtp.sent[-1][2].as_string()

            count = len(smtp.sent)
            mail_cli(
                "reply",
                "--all",
                "--uid-validity",
                str(follow["uid_validity"]),
                str(follow["uid"]),
                task="Reply all",
            )
            cli("mail", "flush")
            replies = smtp.sent[count:]
            assert len(replies) == 3
            assert {entry[1].decode() for entry in replies} == {
                REPLY_TO,
                MAILBOXES["qa"],
                MAILBOXES["review"],
            }
            for _, _, reply in replies:
                assert [a.addr_spec for a in reply["Cc"].addresses] == [
                    MAILBOXES["review"],
                    MAILBOXES["qa"],
                ]
                assert (
                    reply["Bcc"] is None
                    and reply["In-Reply-To"] == "<followup@example.test>"
                )

            for field, header in [
                (
                    "unauthorized-reply-to",
                    b"From: human@example.test\r\nReply-To: unauthorized@example.test",
                ),
                ("unauthorized-from", b"From: unauthorized@example.test"),
            ]:
                append(
                    imap_port,
                    message("human@example.test", "build", field).replace(
                        b"\r\nFrom: human@example.test", b"\r\n" + header
                    ),
                )
                denied = json.loads(mail_cli("search", f"{field}@example.test").stdout)[
                    "messages"
                ][0]["mail"]
                error = mail_cli(
                    "reply",
                    "--uid-validity",
                    str(denied["uid_validity"]),
                    str(denied["uid"]),
                    task="Do not send",
                    success=False,
                )
                assert "reply recipient is not authorized" in error.stderr, error.stderr
            assert not json.loads(cli("mail", "pending").stdout)["queued"]
            monitor = "monitor@example.test"
            archive = "archive@example.test"
            (project / ".agents.d" / "90-observers.json").write_text(
                json.dumps(
                    {"mail": {"cc": [monitor], "bcc": [archive], "to": ["review"]}}
                )
            )
            mail_cli(
                "send",
                "--to",
                "qa",
                "--subject",
                "Denied by agent ID",
                task="Do not send",
                success=False,
            )
            mail_cli(
                "send",
                "--to",
                "stranger@example.test",
                "--subject",
                "Denied by trust",
                task="Do not send",
                success=False,
            )
            count = len(smtp.sent)
            report = json.loads(
                mail_cli(
                    "send",
                    "--to",
                    "human@example.test",
                    "--subject",
                    "Report",
                    task="Done",
                ).stdout
            )
            assert report["recipients"] == 3
            cli("mail", "flush")
            copies = smtp.sent[count:]
            assert {entry[1].decode() for entry in copies} == {
                "human@example.test",
                monitor,
                archive,
            }
            for _, _, copy in copies:
                assert copy["Cc"].addresses[0].addr_spec == monitor
                assert copy["Bcc"] is None and archive not in copy.as_string()
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
    print("mail round trip passed")


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


if __name__ == "__main__":
    main()
