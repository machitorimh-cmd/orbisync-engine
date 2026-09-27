"""Ordinary HTTP contract smoke test; engine TLS is tested in the Rust fixture."""
import hashlib
import hmac
import http.client
from http.server import ThreadingHTTPServer
import json
import threading
import time
import unittest

from service import handler


class RuleServiceTest(unittest.TestCase):
    def test_signed_proposal_rejection_and_bad_signature(self):
        secret = b"fixture-only-rule-secret"
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler(secret, "world"))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            for dx, signature_valid, expected in [(1, True, "accept"), (2, True, "reject"), (1, False, None)]:
                timestamp = int(time.time())
                wire = {"event_id": "command", "event_kind": "input.compute", "timestamp": timestamp,
                        "payload": {"version": 1, "request_id": "command", "world_id": "world",
                                    "rule": "example.move", "component_key": "example.position",
                                    "requester": "owner", "current_entity": {"owner_id": "owner",
                                    "components": {"example.position": {"encoding": "json", "value": {"x": 7}}}},
                                    "intent": {"dx": dx, "x": 999}}}
                raw = json.dumps(wire).encode()
                signature = hmac.new(secret, f"{timestamp}.command.".encode() + raw, hashlib.sha256).hexdigest()
                connection = http.client.HTTPConnection(*server.server_address, timeout=3)
                connection.request("POST", "/compute", raw, {"Content-Type": "application/json",
                    "X-OrbiSync-Event-Id": "command", "X-OrbiSync-Timestamp": str(timestamp),
                    "X-OrbiSync-Signature": "sha256=" + (signature if signature_valid else "invalid")})
                response = connection.getresponse()
                result = json.loads(response.read())
                self.assertEqual(response.status, 200 if signature_valid else 401)
                if signature_valid:
                    self.assertEqual(result["request_id"], "command")
                    self.assertEqual(result["decision"], expected)
                    if expected == "accept":
                        self.assertEqual(result["update"], {"x": 8})
                connection.close()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
