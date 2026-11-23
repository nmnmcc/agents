"""Deterministic stand-in for an agent harness in the live mail test."""

import json
import os
import subprocess
import sys


def main():
    assert "AGENTS_SMOKE_IMAP" not in os.environ
    header, payload = sys.stdin.read().split("\n", 1)
    assert header.startswith("The new message in your inbox follows."), header
    trigger = json.loads(payload)
    server = json.loads(sys.argv[1])["agents-mail"]
    with subprocess.Popen(
        [server["command"], *server["args"]],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    ) as client:

        def rpc(method, ident=None, params=None):
            request = {"jsonrpc": "2.0", "method": method}
            if ident is not None:
                request["id"] = ident
            if params is not None:
                request["params"] = params
            client.stdin.write(json.dumps(request) + "\n")
            client.stdin.flush()
            if ident is not None:
                response = json.loads(client.stdout.readline())
                assert response["id"] == ident and "result" in response, response
                return response["result"]

        def call(ident, name, arguments, error=False):
            result = rpc("tools/call", ident, {"name": name, "arguments": arguments})
            assert result["isError"] == error, result
            if not error:
                return json.loads(result["content"][0]["text"])

        rpc(
            "initialize",
            1,
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mail-test", "version": "1"},
            },
        )
        rpc("notifications/initialized")
        assert {
            "send_mail",
            "list_mail",
            "search_mail",
            "get_mail",
            "get_thread",
            "get_attachment",
            "set_mail_read",
        } <= {tool["name"] for tool in rpc("tools/list", 2)["tools"]}
        mail = trigger["message"]["mail"]
        full = call(3, "get_mail", mail)
        assert full["body"] == trigger["body"] and full["message"]["unread"]
        call(
            4,
            "send_mail",
            {"to": ["unapproved@example.test"], "subject": "Never", "body": "Never"},
            error=True,
        )
        call(5, "set_mail_read", {"mail": mail, "read": True})
        assert not call(6, "get_mail", mail)["message"]["unread"]
        client.stdin.close()
        assert client.wait(timeout=5) == 0
    print("handled")


if __name__ == "__main__":
    main()
