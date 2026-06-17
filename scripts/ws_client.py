#!/usr/bin/env python3
"""Drive a tales-web session programmatically over its WebSocket — no deps.

A minimal, stdlib-only WebSocket client that connects to a running `tales-web`
server, prints the live Claude<->Codex conversation, injects a human note, and
confirms the executor at the gate. Useful for scripting, automation, and
end-to-end testing of the supervisor without a browser.

    tales-web "your task" --no-open &      # start the server
    python3 scripts/ws_client.py 7878      # watch + steer it

The wire protocol (events in, commands out) is documented in
crates/tales-web/src/main.rs (event_to_json / parse_command).
"""
import socket, base64, os, struct, json, sys

HOST = "127.0.0.1"
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7878
# Optional: a note to inject after the drafter's first message.
NOTE = sys.argv[2] if len(sys.argv) > 2 else (
    "Speaking as the human supervisor: prioritize security and abuse tests."
)


def connect():
    s = socket.create_connection((HOST, PORT))
    s.settimeout(180)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall(
        (
            f"GET /ws HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += s.recv(4096)
    assert b"101" in buf.split(b"\r\n")[0], buf[:120]
    return s, buf.split(b"\r\n\r\n", 1)[1]


def frames(s, buf=b""):
    def need(n):
        nonlocal buf
        while len(buf) < n:
            d = s.recv(4096)
            if not d:
                raise EOFError
            buf += d

    while True:
        try:
            need(2)
        except EOFError:
            return
        op = buf[0] & 0x0F
        masked = buf[1] & 0x80
        ln = buf[1] & 0x7F
        i = 2
        if ln == 126:
            need(4); ln = struct.unpack(">H", buf[2:4])[0]; i = 4
        elif ln == 127:
            need(10); ln = struct.unpack(">Q", buf[2:10])[0]; i = 10
        mask = b""
        if masked:
            need(i + 4); mask = buf[i : i + 4]; i += 4
        need(i + ln)
        payload = bytearray(buf[i : i + ln]); buf = buf[i + ln :]
        if masked:
            for j in range(len(payload)):
                payload[j] ^= mask[j % 4]
        if op == 0x8:
            return
        if op == 0x1:
            yield payload.decode("utf-8", "replace")


def send(s, obj):
    data = json.dumps(obj).encode()
    mask = os.urandom(4)
    p = bytearray(data)
    for j in range(len(p)):
        p[j] ^= mask[j % 4]
    ln = len(data)
    h = bytearray([0x81])
    if ln < 126:
        h.append(0x80 | ln)
    elif ln < 65536:
        h += bytes([0x80 | 126]) + struct.pack(">H", ln)
    else:
        h += bytes([0x80 | 127]) + struct.pack(">Q", ln)
    s.sendall(bytes(h) + bytes(mask) + bytes(p))


def main():
    s, leftover = connect()
    labels, recommended, injected = {}, None, False
    for raw in frames(s, leftover):
        try:
            m = json.loads(raw)
        except Exception:
            continue
        k = m.get("kind")
        if k == "agent":
            labels[m["agent"]] = m["label"]
            print(f"[joined] {m['label']}", flush=True)
        elif k == "message":
            who = labels.get(m["agent"], "?")
            print(f"\n<{who}>:\n{m['text']}", flush=True)
            if not injected and who.lower() == "claude":
                print(f"\n  >>> [ME -> agents] {NOTE}", flush=True)
                send(s, {"kind": "say", "text": NOTE})
                injected = True
        elif k == "user":
            print(f"\n<you>: {m['text']}", flush=True)
        elif k == "recommendation":
            recommended = m["executor"]
            print(f"\n[recommend: {m['executor']}]", flush=True)
        elif k == "awaiting":
            exec_choice = os.environ.get("TALES_EXEC") or recommended or "claude"
            print(f"\n  >>> [ME -> gate] confirm executor: {exec_choice}", flush=True)
            send(s, {"kind": "confirm", "executor": exec_choice})
        elif k == "tool":
            print(f"  [tool] {labels.get(m['agent'], '?')}: {m.get('summary', '')}", flush=True)
        elif k == "log" and m.get("level") == "done":
            print("\n[session done]", flush=True)
            break
        elif k == "fatal":
            print(f"\n[fatal] {m['msg']}", flush=True)
            break


if __name__ == "__main__":
    main()
