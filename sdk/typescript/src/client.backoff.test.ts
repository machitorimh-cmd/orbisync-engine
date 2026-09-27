import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { OrbiSyncClient, OrbiSyncConnection } from "./client.js";

describe("reconnect backoff (MRIB-01)", () => {
  function makeConnection(): OrbiSyncConnection {
    const ws = {
      readyState: 1,
      addEventListener: () => {},
      removeEventListener: () => {},
      send: () => {},
      close: () => {},
    } as unknown as WebSocket;
    const serverHello = {
      negotiatedMinor: 0,
      connectionId: "conn-1",
      heartbeatIntervalMs: 20_000n,
      serverTimeUnixMs: BigInt(Date.now()),
      negotiatedCompression: "",
      enabledFeatures: [],
    } as never;
    const connection = new OrbiSyncConnection(
      ws,
      serverHello,
      new OrbiSyncClient({ baseUrl: "http://localhost" }),
    );
    (connection as unknown as { stopHeartbeat(): void }).stopHeartbeat();
    return connection;
  }

  it("uses the exponential upper bound, capped at 30 seconds", () => {
    const connection = makeConnection();
    const compute = (connection as unknown as { _computeBackoffMs(attempt: number): number })._computeBackoffMs;
    const originalRandom = Math.random;
    Math.random = () => 0.5;
    try {
      assert.equal(compute.call(connection, 0), 500);
      assert.equal(compute.call(connection, 1), 1_000);
      assert.equal(compute.call(connection, 5), 15_000);
      assert.equal(compute.call(connection, 6), 15_000);
      assert.equal(compute.call(connection, 100), 15_000);
    } finally {
      Math.random = originalRandom;
    }
  });

  it("supports full-jitter endpoints and never exceeds the cap", () => {
    const connection = makeConnection();
    const compute = (connection as unknown as { _computeBackoffMs(attempt: number): number })._computeBackoffMs;
    const originalRandom = Math.random;
    try {
      Math.random = () => 0;
      assert.equal(compute.call(connection, 0), 0);
      assert.equal(compute.call(connection, 100), 0);

      Math.random = () => 1;
      assert.equal(compute.call(connection, 0), 1_000);
      assert.equal(compute.call(connection, 5), 30_000);
      assert.equal(compute.call(connection, 100), 30_000);
    } finally {
      Math.random = originalRandom;
    }
  });

  it("stops scheduling after the maximum reconnect attempts", () => {
    const ws = {
      readyState: 1,
      addEventListener: () => {},
      removeEventListener: () => {},
      send: () => {},
      close: () => {},
    } as unknown as WebSocket;
    const serverHello = {
      negotiatedMinor: 0,
      connectionId: "conn-1",
      heartbeatIntervalMs: 20_000n,
      serverTimeUnixMs: BigInt(Date.now()),
      negotiatedCompression: "",
      enabledFeatures: [],
    } as never;
    const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
    const connection = new OrbiSyncConnection(ws, serverHello, client);
    const internals = connection as unknown as {
      reconnectAttempt: number;
      reconnectTimer: ReturnType<typeof setTimeout> | null;
      scheduleReconnect(): void;
      stopHeartbeat(): void;
    };
    internals.stopHeartbeat();
    internals.reconnectAttempt = 20;
    internals.scheduleReconnect();
    assert.equal(internals.reconnectTimer, null);
  });
});
