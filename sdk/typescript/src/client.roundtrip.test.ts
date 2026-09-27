import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { decodeEnvelope, encodeEnvelope } from "./client.js";

describe("protobuf round-trip (W-14)", () => {
  it("preserves messageId, sequence and transformInput payload including positionX", () => {
    const env = create(EnvelopeSchema, {
      protocolMajor: 1,
      protocolMinor: 0,
      messageId: "test-msg-123",
      sequence: 42n,
      sentAtUnixMs: 1700000000000n,
      instanceId: "0192d43d-a18a-7fed-8123-0123456789ab",
      payload: {
        case: "transformInput",
        value: {
          entityId: "entity-abc",
          expectedRevision: 5n,
          transform: {
            positionX: 1.5,
            positionY: -2.25,
            positionZ: 3.75,
            rotationX: 0,
            rotationY: 0,
            rotationZ: 0,
            rotationW: 1,
          },
        },
      },
    });

    const bytes = encodeEnvelope(env);

    // Must not be the old stub (empty array). This catches the un-wired SDK.
    assert.ok(bytes.length > 0, "encodeEnvelope must not return empty Uint8Array (stub)");

    const decoded = decodeEnvelope(bytes);

    assert.equal(decoded.messageId, "test-msg-123");
    assert.equal(decoded.sequence, 42n);
    // sequence must be non-zero to prove monotonic counter is wired
    assert.notEqual(decoded.sequence, 0n);
    assert.equal(decoded.instanceId, "0192d43d-a18a-7fed-8123-0123456789ab");
    assert.ok(decoded.payload, "payload must survive round-trip");
    assert.equal(decoded.payload.case, "transformInput");
    if (decoded.payload.case === "transformInput") {
      assert.equal(decoded.payload.value.entityId, "entity-abc");
      assert.equal(decoded.payload.value.expectedRevision, 5n);
      const t = decoded.payload.value.transform;
      assert.ok(t, "transform must be present");
      // Concrete field-level check — proves real protobuf, not dummy echo
      assert.equal(t!.positionX, 1.5);
      assert.equal(t!.positionY, -2.25);
      assert.equal(t!.positionZ, 3.75);
      assert.equal(t!.rotationW, 1);
    }
  });

  it("preserves clientHello oneof and its ticket field", () => {
    const env = create(EnvelopeSchema, {
      protocolMajor: 1,
      protocolMinor: 0,
      messageId: "hello-1",
      sequence: 1n,
      sentAtUnixMs: 1700000000000n,
      instanceId: "",
      payload: {
        case: "clientHello",
        value: {
          supportedMinorMin: 0,
          supportedMinorMax: 0,
          realtimeTicket: "ticket-xyz-789",
          clientName: "@orbisync/client",
          clientVersion: "0.1.0",
          clientType: "desktop",
          supportedCompressions: [],
          supportedFeatures: [],
          resumeToken: "",
        },
      },
    });

    const bytes = encodeEnvelope(env);
    assert.ok(bytes.length > 0);
    const decoded = decodeEnvelope(bytes);
    assert.equal(decoded.messageId, "hello-1");
    assert.equal(decoded.sequence, 1n);
    assert.equal(decoded.payload.case, "clientHello");
    if (decoded.payload.case === "clientHello") {
      assert.equal(decoded.payload.value.realtimeTicket, "ticket-xyz-789");
      assert.equal(decoded.payload.value.clientName, "@orbisync/client");
    }
  });

  it("round-trips heartbeat payload", () => {
    const now = 1700000000123n;
    const env = create(EnvelopeSchema, {
      protocolMajor: 1,
      protocolMinor: 0,
      messageId: "hb-1",
      sequence: 99n,
      sentAtUnixMs: now,
      instanceId: "inst-1",
      payload: {
        case: "heartbeat",
        value: { clientTimeUnixMs: now },
      },
    });
    const decoded = decodeEnvelope(encodeEnvelope(env));
    assert.equal(decoded.payload.case, "heartbeat");
    if (decoded.payload.case === "heartbeat") {
      assert.equal(decoded.payload.value.clientTimeUnixMs, now);
    }
  });
});
