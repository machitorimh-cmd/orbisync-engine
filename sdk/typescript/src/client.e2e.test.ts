import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { spawn, ChildProcess } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocket as WsWebSocket } from "ws";
import { OrbiSyncClient } from "./client.js";

// Use `ws` package's WebSocket in Node to avoid undici's 5s idle timeout that
// closes the socket when no application message is exchanged. The `ws` package
// stays open indefinitely and matches browser behavior for the test. This is
// test-only; the SDK itself uses global WebSocket (browser) or `ws` if available.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
if ((globalThis as any).WebSocket !== WsWebSocket) {
  (globalThis as any).WebSocket = WsWebSocket as unknown as typeof WebSocket;
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

type HelperInfo = {
  proc: ChildProcess;
  addr: string;
  worldId: string;
  instanceId: string;
};

function hasExited(proc: ChildProcess): boolean {
  return proc.exitCode !== null || proc.signalCode !== null;
}

function waitForExit(proc: ChildProcess, timeoutMs: number): Promise<void> {
  if (hasExited(proc)) return Promise.resolve();

  return new Promise((resolve) => {
    const finish = (): void => {
      clearTimeout(timer);
      proc.off("exit", finish);
      resolve();
    };
    const timer = setTimeout(finish, timeoutMs);
    proc.once("exit", finish);
    timer.unref?.();
  });
}

async function stopHelper(proc: ChildProcess): Promise<void> {
  if (hasExited(proc)) return;

  proc.kill("SIGTERM");
  await waitForExit(proc, 1_000);
  if (!hasExited(proc)) {
    proc.kill("SIGKILL");
    await waitForExit(proc, 1_000);
  }
}

async function startHelper(): Promise<HelperInfo> {
  // Build this binary before starting the Node test. Keeping compilation out
  // of the startup timeout avoids an orphaned `cargo run` process on a cold
  // checkout and makes the timeout measure server readiness only.
  const binaryCandidates = [
    path.resolve(__dirname, "../../../target/debug/orbisync-e2e-helper.exe"),
    path.resolve(__dirname, "../../../target/debug/orbisync-e2e-helper"),
    path.resolve(__dirname, "../../target/debug/orbisync-e2e-helper.exe"),
  ];
  const chosen = binaryCandidates.find(existsSync);
  if (!chosen) {
    throw new Error(
      "orbisync-e2e-helper is not built; run `cargo build -p orbisync-e2e-helper` from the repository root",
    );
  }

  const proc = spawn(chosen, [], { stdio: ["ignore", "pipe", "pipe"] });
  proc.stderr?.on("data", (data: Buffer) => {
    process.stderr.write(`[helper] ${data}`);
  });

  try {
    const info = await new Promise<HelperInfo>((resolve, reject) => {
      let stdout = "";
      let settled = false;

      const cleanup = (): void => {
        clearTimeout(timer);
        proc.stdout?.off("data", onStdout);
        proc.off("exit", onExit);
      };
      const fail = (error: Error): void => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(error);
      };
      const onStdout = (data: Buffer): void => {
        stdout += data.toString();
        const match = stdout.match(/READY addr=([^\s]+) worldId=([^\s]+) instanceId=([^\s]+)/);
        if (!match || settled) return;
        settled = true;
        cleanup();
        resolve({
          proc,
          addr: match[1]!,
          worldId: match[2]!,
          instanceId: match[3]!,
        });
      };
      const onError = (error: Error): void => fail(error);
      const onExit = (code: number | null, signal: NodeJS.Signals | null): void => {
        fail(
          new Error(
            `helper exited before READY (code=${String(code)}, signal=${String(signal)}), stdout=${stdout}`,
          ),
        );
      };
      const timer = setTimeout(
        () => fail(new Error("helper start timeout, stdout=" + stdout)),
        15_000,
      );

      proc.stdout?.on("data", onStdout);
      proc.once("error", onError);
      proc.once("exit", onExit);
      timer.unref?.();
    });

    // Give server a moment to be ready for HTTP.
    await sleep(200);
    return info;
  } catch (error) {
    await stopHelper(proc);
    throw error;
  }
}

function uuidv7(): string {
  const timestamp = Date.now();
  const timeHex = timestamp.toString(16).padStart(12, "0");
  const rand = crypto.getRandomValues(new Uint8Array(10));
  // Build bytes
  const bytes = new Uint8Array(16);
  // timestamp 48 bits
  for (let i = 0; i < 6; i++) {
    bytes[i] = parseInt(timeHex.slice(i * 2, i * 2 + 2), 16);
  }
  for (let i = 0; i < 10; i++) bytes[6 + i] = rand[i]!;
  // version 7
  bytes[6] = (bytes[6]! & 0x0f) | 0x70;
  // variant 10xx
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function waitForEvent(
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  instance: any,
  event: string,
  predicate: (v: unknown) => boolean,
  timeoutMs: number,
): Promise<unknown> {
  const timeoutError = new Error(`waitForEvent timeout event=${event}`);
  return new Promise<unknown>((resolve, reject) => {
    const timer = setTimeout(() => {
      instance.off(event, handler);
      reject(timeoutError);
    }, timeoutMs);
    const handler = (v: unknown) => {
      if (predicate(v)) {
        clearTimeout(timer);
        instance.off(event, handler);
        resolve(v);
      }
    };
    instance.on(event, handler);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (timer as any)?.unref?.();
  });
}

// The choice of spawning a Rust binary on 127.0.0.1:0 with FakeWorldDirectoryStore + StubTicketVerifier
// mirrors tests/integration/tests/resume_w18.rs:177-211 (same RealtimeState builder pattern).
// Alternative considered was Docker Compose (scripts/compose-e2e.sh) which needs Postgres + Docker and is
// slower/flakier for a unit-priced SDK job. The in-process fake-store server is deterministic, needs no DB,
// starts in <200ms, and still exercises the real Rust resume path (session_store, decide_resync, token rotation)
// over a real TCP WebSocket. No mock WebSocket or protobuf round-trip is involved.

describe("W-20 reconnect e2e (real server + real socket)", () => {
  it("reconnects, resumes with state intact, and falls back via resync when token invalid", async () => {
    const helper = await startHelper();
    const baseUrl = `http://${helper.addr}`;
    try {
      // Client A joins
      const clientA = new OrbiSyncClient({ baseUrl, wsPath: "/ws" });
      await clientA.auth.login({ loginId: "alice", password: "any" });
      const connA = await clientA.connect();
      const readyHeartbeat = new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => { unsubscribe(); reject(new Error("heartbeat before join timed out")); }, 3000);
        const unsubscribe = connA.onConnectionStateChange(state => {
          if (state.rttMs !== null) { clearTimeout(timer); unsubscribe(); resolve(); }
        }, { emitCurrent: false });
      });
      (connA as unknown as { sendHeartbeat(): void }).sendHeartbeat();
      await readyHeartbeat;
      const instA = await connA.join(helper.instanceId);

      // Register immediately after join: coalesced JoinAccepted/Snapshot frames
      // must reach the public API even before the application has yielded again.
      await waitForEvent(instA, "snapshot", () => true, 4000);
      const tokenBefore = connA._getResumeToken();
      assert.ok(tokenBefore.length > 0, "JoinAccepted must carry resume_token (W-20 scope 2)");
      const revBefore = connA._getLastAppliedRevision();

      // Client B joins same instance and creates an entity while A is connected — ensure delivery works
      const clientB = new OrbiSyncClient({ baseUrl, wsPath: "/ws" });
      await clientB.auth.login({ loginId: "bob", password: "any" });
      const connB = await clientB.connect();
      const instB = await connB.join(helper.instanceId);
      instB.on("error", raw => console.error("peer error code:", (raw as { code?: string }).code));
      await waitForEvent(instB, "snapshot", () => true, 4000);

      const sentEvent = waitForEvent(instA, "domainEvent", () => true, 4000);
      const receivedEvent = waitForEvent(instB, "domainEvent", () => true, 4000);
      instA.sendDomainEvent({
        eventType: "custom.chat.message",
        data: { text: "こんにちは", sender_user_id: "forged-user" },
      });
      const [sent, received] = await Promise.all([sentEvent, receivedEvent]) as Array<{
        eventId: string; eventType: string; instanceRevision: bigint;
        data: { text: string; sender_user_id: string; sender_presence_id: string };
      }>;
      assert.equal(received!.eventId, sent!.eventId);
      assert.equal(received!.eventType, "custom.chat.message");
      assert.equal(received!.data.text, "こんにちは");
      assert.equal(received!.data.sender_user_id, instA.getJoinInfo().userId);
      assert.equal(received!.data.sender_presence_id, instA.getJoinInfo().presenceId);
      assert.ok(received!.instanceRevision > 0n);

      // B sends a transform that A should receive (proves state exchange)
      // EntityId must be a valid UUIDv7 (server validates via EntityId::parse, ADR-001)
      const entity1 = uuidv7();
      const entity1Seen = waitForEvent(instA, "entityUpdated", (v) => (v as { entityId?: string }).entityId === entity1, 4000);
      instB.sendTransform({ entityId: entity1, position: { x: 1, y: 0, z: 0 } });
      await entity1Seen;

      // Now kill A's transport UNEXPECTEDLY (not close 1000) — handler at client.ts: close(1000) returns early
      // so we use 1011/4000 to trigger scheduleReconnect
      const queuedEntity = uuidv7();
      const queuedEntitySeen = waitForEvent(instB, "entityUpdated", (v) => (v as { entityId?: string }).entityId === queuedEntity, 4000);
      const ensureToken = clientA.ensureValidAccessToken.bind(clientA);
      let releaseReconnect!: () => void;
      const reconnectGate = new Promise<void>(resolve => { releaseReconnect = resolve; });
      clientA.ensureValidAccessToken = async () => { await reconnectGate; await ensureToken(); };
      const resumedSnapshot = waitForEvent(instA, "snapshot", () => true, 5000);
      let appliedCursor: bigint | undefined;
      let activeCursor: bigint | undefined;
      const observeSnapshot = () => { appliedCursor = connA.getConnectionState().lastAppliedRevision; };
      instA.on("snapshot", observeSnapshot);
      const stopObservingState = connA.onConnectionStateChange(state => {
        if (state.phase === "connected" && state.sessionState === "Active" && activeCursor === undefined) {
          activeCursor = state.lastAppliedRevision;
        }
      }, { emitCurrent: false });
      // Simulate a successful local socket write whose bytes never reach the
      // server. The pending event must survive and arrive after resume.
      const droppedSocket = connA._getWs();
      const originalSend = droppedSocket.send.bind(droppedSocket);
      droppedSocket.send = () => {};
      const retriedEcho = waitForEvent(instA, "domainEvent", raw => (raw as { data?: { text?: string } }).data?.text === "lost-before-server", 7000);
      const retriedPeer = waitForEvent(instB, "domainEvent", raw => (raw as { data?: { text?: string } }).data?.text === "lost-before-server", 7000);
      instA.sendDomainEvent({ eventType: "custom.chat.message", data: { text: "lost-before-server" } });
      droppedSocket.send = originalSend;
      connA._forceCloseTransport(1011, "test unexpected close");
      instA.sendTransform({ entityId: queuedEntity, position: { x: 1, y: 0, z: 0 } });
      await sleep(200); // let server mark A as disconnected and enter grace window
      const missedTexts: string[] = [];
      instA.on("domainEvent", raw => {
        const value = raw as { data?: { text?: string } };
        if (value.data?.text?.startsWith("offline-")) missedTexts.push(value.data.text);
      });
      for (const text of ["offline-first", "offline-second"]) {
        const echoed = waitForEvent(instB, "domainEvent", raw => (raw as { data?: { text?: string } }).data?.text === text, 4000);
        instB.sendDomainEvent({ eventType: "custom.chat.message", data: { text } });
        await echoed;
      }
      const offlineUpdate = waitForEvent(instB, "entityUpdated", v => (v as { entityId?: string }).entityId === entity1, 4000);
      instB.sendEntityCommand({ entityId: entity1, operation: "update", args: { component_key: "test.shared", value: 42 } });
      await offlineUpdate;

      // While A is in Backoff/Reconnecting, B creates a second entity — this should be replayed to A on resume
      const entity2 = uuidv7();
      // Send entity2 from B while A is disconnected
      const entity2Accepted = waitForEvent(instB, "entityUpdated", v => (v as { entityId?: string }).entityId === entity2, 4000);
      instB.sendTransform({ entityId: entity2, position: { x: 2, y: 0, z: 0 } });
      await entity2Accepted;
      releaseReconnect();
      // Wait for A to reconnect and rotate token (poll)
      let tokenAfterResume = tokenBefore;
      for (let i = 0; i < 30; i++) {
        await sleep(200);
        tokenAfterResume = connA._getResumeToken();
        if (tokenAfterResume !== tokenBefore && tokenAfterResume.length > 0) break;
      }
      console.log(`tokenBefore len=${tokenBefore.length} tokenAfter len=${tokenAfterResume.length} rotated=${tokenBefore !== tokenAfterResume}`);
      assert.ok(tokenAfterResume.length > 0, "ResumeAccepted must carry rotated token");
      assert.notEqual(tokenAfterResume, tokenBefore, "rotated token must differ (single-use)");
      clientA.ensureValidAccessToken = ensureToken;
      await queuedEntitySeen;
      await Promise.all([retriedEcho, retriedPeer]);
      const resumed = await resumedSnapshot as { data: Uint8Array; instanceRevision: bigint };
      instA.off("snapshot", observeSnapshot);
      stopObservingState();
      assert.equal(appliedCursor, resumed.instanceRevision, "applied cursor equals the server's snapshot revision");
      assert.equal(activeCursor, resumed.instanceRevision, "Active is published only after replay and snapshot application");
      assert.equal(connA.getConnectionState().sessionState, "Active");
      const resumedState = JSON.parse(Buffer.from(resumed.data).toString("utf8"));
      assert.deepEqual(missedTexts, ["offline-first", "offline-second"], "missed messages replay once in order before the snapshot");
      const restoredEntity = resumedState.entities.find((e: { entity_id: string }) => e.entity_id === entity1);
      assert.equal(JSON.parse(Buffer.from(restoredEntity.components["test.shared"]).toString("utf8")).value, 42);
      const postResumeUpdate = waitForEvent(instB, "entityUpdated", v => (v as { entityId?: string }).entityId === entity1, 4000);
      instA.sendEntityCommand({ entityId: entity1, operation: "update", args: { component_key: "test.shared", value: 43 } });
      await postResumeUpdate; // No explicit revision: must use the resumed snapshot.
      // Also ensure A's reconnect succeeded and it can receive new deltas
      // B sends another entity after reconnect, A should receive it as normal delta
      const entity2b = uuidv7();
      const entity2bSeen = waitForEvent(instA, "entityUpdated", (v) => (v as { entityId?: string }).entityId === entity2b, 4000);
      instB.sendTransform({ entityId: entity2b, position: { x: 2, y: 0, z: 0 } });
      await entity2bSeen;

      // Verify sequence was reset (new connection) and still monotonic: next send should succeed
      instA.sendTransform({ entityId: uuidv7(), position: { x: 3, y: 0, z: 0 } });
      await sleep(300);
      assert.equal(connA._getWs().readyState, 1, "WebSocket should be OPEN after resume");

      // --- Resync fallback path: invalidate token and force another unexpected close ---
      // Force resume to fail (invalid token) and assert SDK recovers via fresh join, not death
      connA._setResumeToken("invalid-token-000000000000000000000000000000000000");
      const freshSnapshot = waitForEvent(instA, "snapshot", () => true, 6000);
      // Also ensure B creates another entity that fresh snapshot should contain?
      // Instead, just kill and wait for fresh join
      connA._forceCloseTransport(4000, "test invalid token resync");
      await sleep(4000); // allow backoff + fresh join
      const fresh = await freshSnapshot as { data: Uint8Array };
      const freshState = JSON.parse(Buffer.from(fresh.data).toString("utf8")) as { entities: Array<{ entity_id: string }> };
      assert.ok(freshState.entities.some(entity => entity.entity_id === entity1));

      assert.equal(connA._getWs().readyState, 1, "WebSocket should be OPEN after resync fallback");
      const tokenAfterResync = connA._getResumeToken();
      assert.ok(tokenAfterResync.length > 0, "fresh join after resync must issue new token");
      assert.notEqual(tokenAfterResync, "invalid-token-000000000000000000000000000000000000");

      // Verify fresh join still allows sends
      const entity3 = uuidv7();
      const entity3Seen = waitForEvent(instB, "entityUpdated", (v) => (v as { entityId?: string }).entityId === entity3, 4000);
      instA.sendTransform({ entityId: entity3, position: { x: 4, y: 0, z: 0 } });
      await entity3Seen;

      // --- Heartbeat missed path: trigger 3 missed acks → same reconnect flow ---
      const wsBeforeHb = connA._getWs();
      connA._testOnHeartbeatMissed();
      connA._testOnHeartbeatMissed();
      connA._testOnHeartbeatMissed(); // third triggers close(4000) + scheduleReconnect
      await sleep(3500);
      assert.equal(connA._getWs().readyState, 1, "heartbeat missed should trigger reconnect and recover");
      assert.notEqual(connA._getWs(), wsBeforeHb, "WebSocket should have been replaced after heartbeat reconnect");

      // Verify RTT was captured at least once (handleHeartbeatAck stores it)
      // Do a heartbeat round-trip by waiting for interval? Instead just check that reconnect kept working
      instA.sendTransform({ entityId: uuidv7(), position: { x: 5, y: 0, z: 0 } });
      await sleep(300);

      await connA.disconnect();
      await connB.disconnect();
      await sleep(200);
    } finally {
      await stopHelper(helper.proc);
    }
  });
});
