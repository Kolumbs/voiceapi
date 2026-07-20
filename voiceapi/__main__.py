#!/usr/bin/env python3
"""Command-line client for the voiceapi WebSocket API.

Standard library only — no installs needed on the Pi.

    python3 -m voiceapi health
    python3 -m voiceapi list_sms
    python3 -m voiceapi read_sms -d '{"index": 3}'
    python3 -m voiceapi set_config -d '{"at_port": "/dev/ttyUSB5"}'
    python3 -m voiceapi reconnect
    python3 -m voiceapi list_errors -d '{"limit": 5}'

The operation name is passed through untouched, so this client never needs
updating when the server gains an op; an unknown one just comes back as
`bad_request`.

Exit status is 0 when the response has "ok": true, otherwise 1 — so it composes
with shell scripting.
"""

import argparse
import base64
import json
import os
import socket
import ssl
import struct
import sys
from urllib.parse import urlparse

DEFAULT_URL = os.environ.get("VOICEAPI_URL", "ws://127.0.0.1:9500/")


def connect(url, timeout):
    """Open a WebSocket connection. Returns (socket, leftover_bytes)."""
    parts = urlparse(url)
    secure = parts.scheme == "wss"
    host = parts.hostname or "127.0.0.1"
    port = parts.port or (443 if secure else 9500)
    path = parts.path or "/"
    if parts.query:
        path = f"{path}?{parts.query}"

    sock = socket.create_connection((host, port), timeout=timeout)
    if secure:
        sock = ssl.create_default_context().wrap_socket(sock, server_hostname=host)

    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall(
        (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )

    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise RuntimeError("connection closed during handshake")
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    status = head.split(b"\r\n")[0].decode(errors="replace")
    if "101" not in status:
        raise RuntimeError(f"handshake failed: {status}")
    return sock, rest


def send_text(sock, text):
    payload = text.encode()
    mask = os.urandom(4)
    header = bytearray([0x81])  # FIN + text
    n = len(payload)
    if n < 126:
        header.append(0x80 | n)
    elif n < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", n)
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", n)
    header += mask
    sock.sendall(bytes(header) + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))


class Frames:
    def __init__(self, sock, initial=b""):
        self.sock = sock
        self.buf = initial

    def _need(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("connection closed by server")
            self.buf += chunk

    def next(self):
        """Return (opcode, payload) for the next frame."""
        self._need(2)
        opcode = self.buf[0] & 0x0F
        masked = self.buf[1] & 0x80
        length = self.buf[1] & 0x7F
        off = 2
        if length == 126:
            self._need(4)
            length = struct.unpack("!H", self.buf[2:4])[0]
            off = 4
        elif length == 127:
            self._need(10)
            length = struct.unpack("!Q", self.buf[2:10])[0]
            off = 10
        mask = b""
        if masked:
            self._need(off + 4)
            mask = self.buf[off:off + 4]
            off += 4
        self._need(off + length)
        payload = self.buf[off:off + length]
        if masked:
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.buf = self.buf[off + length:]
        return opcode, payload


def main():
    ap = argparse.ArgumentParser(
        prog="python3 -m voiceapi",
        description="Send one operation to voiceapi and print the response.",
    )
    ap.add_argument("op", help="operation, e.g. health, list_sms, set_config, reconnect")
    ap.add_argument(
        "-d",
        "--data",
        default=None,
        help='JSON object of extra fields, e.g. \'{"index": 3}\'',
    )
    ap.add_argument("--url", default=DEFAULT_URL,
                    help=f"ws:// or wss:// endpoint (default {DEFAULT_URL})")
    ap.add_argument("--id", type=int, default=1, help="request id to send (default 1)")
    ap.add_argument("--timeout", type=float, default=20.0, help="seconds (default 20)")
    args = ap.parse_args()

    request = {"id": args.id, "op": args.op}
    if args.data:
        try:
            extra = json.loads(args.data)
        except json.JSONDecodeError as exc:
            sys.exit(f"error: -d is not valid JSON: {exc}")
        if not isinstance(extra, dict):
            sys.exit("error: -d must be a JSON object")
        request.update(extra)

    sock, rest = connect(args.url, args.timeout)
    frames = Frames(sock, rest)
    send_text(sock, json.dumps(request))

    while True:
        opcode, payload = frames.next()
        if opcode == 0x1:  # text
            text = payload.decode(errors="replace")
            try:
                body = json.loads(text)
            except json.JSONDecodeError:
                print(text)
                return 1
            print(json.dumps(body, indent=2, sort_keys=True))
            return 0 if body.get("ok") else 1
        if opcode == 0x8:  # close
            print("server closed the connection without responding", file=sys.stderr)
            return 1
        # ping/pong/binary: keep waiting for the response


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except Exception as exc:  # noqa: BLE001 - CLI: report, don't traceback
        sys.exit(f"error: {exc}")
