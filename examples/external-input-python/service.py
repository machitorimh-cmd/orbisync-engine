"""Application-owned example rule. Python 3.11+, standard library only.

Terminate public HTTPS at your proxy, forwarding raw bodies/headers unchanged,
or pass --cert/--key. This service computes proposals; it never commits a move.
"""
import argparse
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
import ssl
import time


def compute(wire, world_id):
    p = wire["payload"]
    request_id = p["request_id"]
    result = {"version": 1, "request_id": request_id}
    if (wire["event_kind"] != "input.compute" or p["version"] != 1
            or wire["event_id"] != request_id or p["world_id"] != world_id
            or p["rule"] != "example.move" or p["component_key"] != "example.position"):
        return {**result, "decision": "reject", "reason": "unsupported contract or world"}
    entity = p["current_entity"]
    # This example chooses an owner-only application policy, even if an engine
    # operator has granted broader update permissions.
    if p["requester"] != entity["owner_id"]:
        return {**result, "decision": "reject", "reason": "owner required"}
    dx = p["intent"].get("dx")
    if type(dx) not in (int, float) or not math.isfinite(dx) or abs(dx) > 1:
        return {**result, "decision": "reject", "reason": "dx must be between -1 and 1"}
    component = entity["components"].get("example.position")
    if component is None:
        x = 0
    elif component["encoding"] == "json":
        x = component["value"]["x"]
    else:
        return {**result, "decision": "reject", "reason": "invalid canonical position"}
    if type(x) not in (int, float) or not math.isfinite(x) or abs(x + dx) > 2**53 - 1:
        return {**result, "decision": "reject", "reason": "invalid canonical position"}
    return {**result, "decision": "accept", "update": {"x": x + dx}}


def authenticate(raw, headers, secret):
    timestamp = headers["X-OrbiSync-Timestamp"]
    event_id = headers["X-OrbiSync-Event-Id"]
    if not timestamp or not event_id or abs(time.time() - int(timestamp)) > 60:
        return False
    expected = "sha256=" + hmac.new(secret, f"{timestamp}.{event_id}.".encode() + raw,
                                    hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, headers.get("X-OrbiSync-Signature", ""))


def handler(secret, world_id):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass  # Do not log canonical state, credentials or intent.

        def do_POST(self):
            self.connection.settimeout(5)
            status, response = 400, {"error": "invalid_request"}
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if self.path != "/compute" or not 0 < length <= 1024 * 1024:
                    raise ValueError("path or body size")
                raw = self.rfile.read(length)
                if not authenticate(raw, self.headers, secret):
                    status, response = 401, {"error": "invalid_signature"}
                else:
                    wire = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
                    if (str(wire["timestamp"]) != self.headers["X-OrbiSync-Timestamp"]
                            or wire["event_id"] != self.headers["X-OrbiSync-Event-Id"]):
                        raise ValueError("header correlation")
                    response = compute(wire, world_id)
                    status = 200
            except (ValueError, KeyError, TypeError, OverflowError):
                pass
            body = json.dumps(response, allow_nan=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--world-id", required=True)
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=9443)
    parser.add_argument("--cert")
    parser.add_argument("--key")
    args = parser.parse_args()
    secret = os.environ["ORBISYNC_RULE_SECRET"].encode()
    if not secret or bool(args.cert) != bool(args.key):
        parser.error("nonempty secret and paired cert/key required")
    server = ThreadingHTTPServer((args.bind, args.port), handler(secret, args.world_id))
    if args.cert:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(args.cert, args.key)
        server.socket = context.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
