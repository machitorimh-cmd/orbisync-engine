// Copy into extracted starter outside the checkout; Node 24 strips types.
import assert from "node:assert/strict";
import { test } from "node:test";
import { spawn } from "node:child_process";
import { realpath } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { connectSession } from "@orbisync/client";
import { movementView } from "@orbisync/client";

async function until(predicate: () => boolean) {
  const deadline = Date.now() + 10_000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("Fixture state timeout");
    await new Promise(r => setTimeout(r, 20));
  }
}

test("starter authenticates, joins, predicts, reflects input and reconnects on real wire", { timeout: 30_000 }, async () => {
  const initialWebSocket = globalThis.WebSocket;
  assert.equal(typeof initialWebSocket, "function", "Node 24 native WebSocket is required");
  const installed = await realpath(fileURLToPath(import.meta.resolve("@orbisync/client")));
  assert.ok(installed.startsWith(await realpath("node_modules")), "package resolves inside consumer");
  assert.ok(process.env.ORBISYNC_E2E_HELPER, "Supply independent verified fixture binary");
  const helper = spawn(process.env.ORBISYNC_E2E_HELPER, [], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  let startError: Error | undefined;
  helper.on("error", error => { startError = error; });
  helper.stdout.on("data", data => { output += data.toString(); });
  helper.stderr.resume();
  let session: Awaited<ReturnType<typeof connectSession>> | undefined;
  let view: Awaited<ReturnType<typeof movementView>> | undefined;
  try {
    await until(() => { if (startError) throw startError; return output.includes("READY"); });
    const info = /READY addr=(\S+) worldId=(\S+) instanceId=(\S+)/.exec(output)!;
    const options = { baseUrl: `http://${info[1]}`, instanceId: info[3], authentication: { method: "local" as const, loginId: "fixture", password: "fixture-only" } };
    await assert.rejects(connectSession({ ...options, authentication: { method: "guest" } }), /not enabled/);
    let failedInstance: Awaited<ReturnType<typeof connectSession>>["instance"] | undefined;
    await assert.rejects(connectSession({ ...options, onState: instance => {
      failedInstance = instance;
      throw new Error("fixture setup callback");
    } }), /fixture setup callback/);
    assert.equal(failedInstance?.syncStatus, "closed");
    const statuses: string[] = [];
    session = await connectSession({ ...options, onState: instance => statuses.push(instance.syncStatus) });
    assert.equal(typeof globalThis.WebSocket, "function");
    assert.equal(globalThis.WebSocket, initialWebSocket);
    assert.equal(session.instance.syncStatus, "ready");
    const entityId = "01900000-0000-7000-8000-000000000001";
    session.instance.sendEntityCommand({ entityId, operation: "spawn", args: { kind: "object", visibility: "global", position_x: 0, position_y: 0, position_z: 0 } });
    await until(() => session!.instance.state.entities.has(entityId));
    view = await movementView(session.instance, entityId, entityId);
    const receipt = view.step(1);
    assert.equal(view.frame().predicted, 1);
    assert.equal(view.frame().authoritative, 0);
    assert.equal((await receipt).status, "accepted");
    assert.equal(view.frame().authoritative, 1);
    assert.equal((await view.step(2)).status, "rejected");
    assert.equal(view.frame().predicted, 1);
    const oldSocket = session.connection._getWs();
    session.connection._forceCloseTransport(4000, "fixture disconnect");
    await until(() => statuses.includes("reconnecting"));
    await session.instance.ready();
    assert.notEqual(session.connection._getWs(), oldSocket);
    assert.equal(view.frame().authoritative, 1);
    assert.equal((await view.step(-1)).status, "accepted");
    assert.equal(view.frame().authoritative, 0);
    // Run the actual documented CLI too, using the entity provisioned above.
    const cli = spawn(process.execPath, ["dist/main.js"], {
      env: { ...process.env, SERVER_URL: options.baseUrl, INSTANCE_ID: options.instanceId,
        AUTH_METHOD: "local", LOGIN_ID: "fixture", PASSWORD: "fixture-only", ENTITY_ID: entityId },
      windowsHide: true, stdio: ["pipe", "pipe", "pipe"],
    });
    let cliOutput = "";
    cli.stdout.on("data", data => { cliOutput += data.toString(); });
    cli.stderr.resume();
    try {
      const exit = new Promise<number | null>((resolve, reject) => {
        cli.on("error", reject);
        cli.on("exit", resolve);
      });
      await until(() => cliOutput.includes("Commands:"));
      cli.stdin.end("1\nquit\n");
      assert.equal(await exit, 0);
      assert.match(cliOutput, /input: accepted/);
      assert.ok(!cliOutput.includes("fixture-only"));
      await until(() => view!.frame().authoritative === 1);
    } finally { cli.kill(); }
    view.dispose();
    await session.leave();
    assert.equal(session.instance.syncStatus, "closed");
    assert.throws(() => view!.step(1), /live, ready/);
  } finally {
    view?.dispose();
    await session?.leave();
    helper.kill();
  }
});
