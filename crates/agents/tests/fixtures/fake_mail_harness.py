"""Deterministic stand-in for an agent harness in the live mail test."""

import json
import os
import subprocess
import sys


def main():
    assert "AGENTS_SMOKE_IMAP" not in os.environ
    prompt = sys.stdin.read()
    assert "Recent unread mail follows." in prompt
    agent = sys.argv[1]
    servers = json.loads(sys.argv[2])
    server = servers["agents-mail"]
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
        tools = {tool["name"] for tool in rpc("tools/list", 2)["tools"]}
        assert {
            "send_mail",
            "list_mail",
            "search_mail",
            "get_mail",
            "get_thread",
            "get_attachment",
            "set_mail_read",
        } <= tools
        if agent == "build":
            if "Inspect inbox." in prompt:
                assert "fulltext-payload" in prompt and "private-reviewer" not in prompt
                result = call(3, "search_mail", {"query": "fulltext-payload"})
                assert len(result["messages"]) == 1
                mail = result["messages"][0]["mail"]
                full = call(4, "get_mail", mail)
                assert "评审" in full["body"]
                assert full["attachments"][0]["name"] == "notes.txt"
                assert (
                    call(5, "get_attachment", {"mail": mail, "index": 0})[
                        "content_base64"
                    ]
                    == "aGVsbG8="
                )
                assert len(call(6, "get_thread", mail)["messages"]) == 2
                assert not call(7, "search_mail", {"query": "private-reviewer"})[
                    "messages"
                ]
                call(8, "get_mail", {**mail, "uid": mail["uid"] + 2}, error=True)
                assert (
                    call(9, "search_mail", {"query": "评审"})["messages"][0]["mail"]
                    == mail
                )
                assert call(10, "get_mail", mail)["message"]["unread"]
                call(11, "set_mail_read", {"mail": mail, "read": True})
                assert not call(12, "get_mail", mail)["message"]["unread"]
            call(
                13,
                "send_mail",
                {
                    "to": ["review"],
                    "bcc": ["unapproved"],
                    "subject": "Never",
                    "body": "Never",
                },
                error=True,
            )
            if "MCP reply." in prompt:
                mail = call(14, "search_mail", {"query": "fulltext-payload"})[
                    "messages"
                ][0]["mail"]
                assert (
                    call(
                        15,
                        "send_mail",
                        {"in_reply_to": mail, "bcc": ["audit"], "body": "MCP reply"},
                    )["recipients"]
                    == 2
                )
            else:
                assert call(
                    14,
                    "send_mail",
                    {
                        "agent": "review",
                        "subject": "Review it",
                        "body": "Please review",
                    },
                )["queued"]
            print("built")
        else:
            assert agent == "review"
            assert "Please review" in prompt and "fulltext-payload" not in prompt
            assert call(3, "search_mail", {"query": "Please review"})["messages"]
            print("reviewed")
        client.stdin.close()
        assert client.wait(timeout=5) == 0


if __name__ == "__main__":
    main()
