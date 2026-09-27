import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { OrbiSyncInstance, createUuidV7, decodeEnvelope } from "./client.js";

class FakeWebSocket {
  readonly sent: Uint8Array[] = [];
  readonly bufferedAmount = 0;
  readonly readyState = 1;

  send(data: Uint8Array): void {
    this.sent.push(new Uint8Array(data));
  }
}

function versionOf(value: string): string {
  return value.split("-")[2]?.[0] ?? "";
}

describe("UUIDv7 and reliable entity revisions", () => {
  it("generates canonical UUIDv7 values with the requested timestamp", () => {
    const timestamp = 1_700_000_000_123;
    const value = createUuidV7(timestamp);

    assert.match(value, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    assert.equal(versionOf(value), "7");
    assert.equal(Number.parseInt(value.replaceAll("-", "").slice(0, 12), 16), timestamp);
  });

  it("uses UUIDv7 command IDs and advances the next command revision", () => {
    const ws = new FakeWebSocket();
    let sequence = 0;
    const instance = new OrbiSyncInstance(
      ws as unknown as WebSocket,
      "0199f4c2-5f5d-7c8a-9123-0123456789ab",
      () => ++sequence,
    );
    const entityId = "0199f4c2-5f5d-7c8a-9123-1123456789ab";

    instance.sendEntityCommand({
      entityId,
      operation: "spawn",
      args: { kind: "object", visibility: "global" },
    });
    const spawn = decodeEnvelope(ws.sent[0]!);
    assert.equal(spawn.payload.case, "entityCommand");
    if (spawn.payload.case !== "entityCommand") assert.fail("expected entityCommand");
    assert.equal(versionOf(spawn.messageId), "7");
    assert.equal(versionOf(spawn.payload.value.commandId), "7");

    instance._dispatch(create(EnvelopeSchema, {
      protocolMajor: 1,
      protocolMinor: 0,
      messageId: createUuidV7(),
      sequence: 1n,
      sentAtUnixMs: BigInt(Date.now()),
      instanceId: "0199f4c2-5f5d-7c8a-9123-0123456789ab",
      payload: {
        case: "entityCommand",
        value: {
          commandId: spawn.payload.value.commandId,
          entityId,
          expectedRevision: 1n,
          operation: "spawn",
          arguments: { kind: "object", visibility: "global" },
        },
      },
    }));

    instance.sendEntityCommand({
      entityId,
      operation: "update",
      args: { component_key: "reference.label", value: "updated" },
    });
    const update = decodeEnvelope(ws.sent[1]!);
    assert.equal(update.payload.case, "entityCommand");
    if (update.payload.case !== "entityCommand") assert.fail("expected entityCommand");
    assert.equal(update.payload.value.expectedRevision, 1n);
  });
});
