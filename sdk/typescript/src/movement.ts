import { PredictedInput, RemoteInterpolator, type OrbiSyncInstance, type SyncedEntity } from "./client.js";

// Application-owned schema and prediction policy, matching the provisioned example.move contract.
function position(entity: SyncedEntity): number {
  const component = entity.properties["example.position"];
  return component ? Number((component as { value: { x: number } }).value.x) : 0;
}

/** Call with an already joined instance and visible local/remote entity IDs. */
export async function movementView(instance: OrbiSyncInstance, localId: string, remoteId: string) {
  await instance.ready();
  const local = new PredictedInput(instance, localId, "example.move", position,
    (x, intent) => x + Number(intent.dx));
  const remote = new RemoteInterpolator<number>((a, b, t) => a + (b - a) * t);
  const capture = () => {
    const entity = instance.state.entities.get(remoteId);
    if (instance.syncStatus !== "ready" || !entity) { remote.reset(); return; }
    remote.push(entity.instanceRevision, performance.now(), position(entity));
  };
  const reset = () => { remote.reset(); capture(); };
  const bindings = [
    ["entityUpdated", capture], ["entitySpawned", capture], ["entityDeleted", capture],
    ["snapshot", reset], ["syncStateChanged", reset],
  ] as const;
  for (const [event, handler] of bindings) instance.on(event, handler);
  capture();
  return {
    local,
    // On a key press: if (!view.local.pendingRequest) void view.step(1);
    // The display changes synchronously; await the receipt before another step.
    step: (dx: number) => local.submit({ dx }),
    frame: () => ({ authoritative: local.authoritative, predicted: local.display,
      remote: remote.sample(performance.now()) }),
    dispose: () => {
      local.dispose();
      for (const [event, handler] of bindings) instance.off(event, handler);
    },
  };
}
