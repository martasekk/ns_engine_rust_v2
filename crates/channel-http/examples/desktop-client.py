#!/usr/bin/env python3
"""A desktop client, both ways, in one file and no dependencies.

    python desktop-client.py socket  --token "$TOKEN"           # the TCP way in
    python desktop-client.py request --token "$TOKEN" "hello"   # one message

The socket mode is what a desktop app does: hold the connection, read replies
as they arrive. The request mode is what a script does: say one thing, get the
answer back in the response. Both reach the same conversation — the token
decides the session, so the same credential from both is one conversation with
two windows on it.

Written against `ns-app serve`; nothing here is specific to Python, and the
whole protocol is three JSON objects (docs/connecting-clients.md §1).
"""

import argparse
import json
import socket
import sys
import threading
import urllib.request

DEFAULT_SOCKET = ("127.0.0.1", 7375)
DEFAULT_HTTP = "http://127.0.0.1:8787"


def socket_mode(host: str, port: int, token: str) -> None:
    """Hold a socket open: type to send, replies print as they arrive."""
    with socket.create_connection((host, port)) as sock:
        stream = sock.makefile("rw", encoding="utf-8", newline="\n")

        # The hello is the first line. `session` is ignored unless the server
        # runs the shared token, which is loopback-only development.
        stream.write(json.dumps({"token": token}) + "\n")
        stream.flush()

        def listen() -> None:
            for line in stream:
                line = line.strip()
                if not line:
                    continue
                reply = json.loads(line)
                # The session id is the server's; printing it once is how you
                # learn which conversation this credential names.
                print(f"\n< {reply['text']}\n> ", end="", flush=True)
            print("\nthe server closed the connection")

        # A refused hello is closed with nothing sent back, so a connection
        # that ends here means the token was not accepted.
        threading.Thread(target=listen, daemon=True).start()

        try:
            while True:
                what = input("> ").strip()
                if not what:
                    continue
                stream.write(json.dumps({"text": what}) + "\n")
                stream.flush()
        except (EOFError, KeyboardInterrupt):
            print()


def request_mode(base: str, token: str, text: str) -> None:
    """One message, one reply, nothing held open."""
    request = urllib.request.Request(
        base.rstrip("/") + "/v1/messages",
        data=json.dumps({"text": text}).encode(),
        headers={"authorization": f"Bearer {token}", "content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(request) as response:
            answered = json.load(response)
        print(answered["text"])
    except urllib.error.HTTPError as e:
        # 504 is not a failure: the turn is still running, and its answer is
        # in the log and on any chat window holding the session.
        detail = e.read().decode(errors="replace")
        print(f"{e.code}: {detail}", file=sys.stderr)
        sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["socket", "request"])
    parser.add_argument("text", nargs="?", help="what to say, in request mode")
    parser.add_argument("--token", required=True, help="a token this company's back end minted")
    parser.add_argument("--host", default=DEFAULT_SOCKET[0])
    parser.add_argument("--port", type=int, default=DEFAULT_SOCKET[1])
    parser.add_argument("--http", default=DEFAULT_HTTP)
    args = parser.parse_args()

    if args.mode == "socket":
        socket_mode(args.host, args.port, args.token)
    else:
        if not args.text:
            parser.error("request mode needs something to say")
        request_mode(args.http, args.token, args.text)


if __name__ == "__main__":
    main()
