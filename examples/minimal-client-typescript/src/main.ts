import { connectSession, movementView, type Authentication } from "@orbisync/client";
import { createInterface } from "node:readline/promises";

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`Set ${name}`);
  return value;
}

function authentication(): Authentication {
  switch (required("AUTH_METHOD")) {
    case "guest": return { method: "guest" };
    case "local": return { method: "local", loginId: required("LOGIN_ID"), password: required("PASSWORD") };
    case "name_only": return { method: "name_only", displayName: required("DISPLAY_NAME") };
    case "external": return { method: "external", token: required("EXTERNAL_TOKEN") };
    default: throw new Error("AUTH_METHOD must be local, guest, name_only, or external");
  }
}

async function main() {
  const entityId = required("ENTITY_ID");
  // SDK handles the realtime_ticket / expires_in contract internally.
  const session = await connectSession({
    baseUrl: required("SERVER_URL"),
    instanceId: required("INSTANCE_ID"), authentication: authentication(),
    onState: instance => console.log("sync:", instance.syncStatus, "revision:", String(instance.state.revision)),
  });
  let view: Awaited<ReturnType<typeof movementView>> | undefined;
  let terminal: ReturnType<typeof createInterface> | undefined;
  const stop = () => terminal?.close();
  try {
    if (!session.instance.state.entities.has(entityId)) throw new Error("ENTITY_ID must be an existing visible entity owned by this identity");
    view = await movementView(session.instance, entityId, process.env.REMOTE_ENTITY_ID ?? entityId);
    terminal = createInterface({ input: process.stdin, output: process.stdout });
    process.on("SIGINT", stop);
    process.on("SIGTERM", stop);
    console.log("Commands: -1, 0, 1, frame, retry, canonical, quit. Wait for each receipt.");
    for await (const line of terminal) {
      const command = line.trim();
      if (command === "quit") break;
      try {
        let result;
        if (["-1", "0", "1"].includes(command)) {
          const receipt = view.step(Number(command));
          console.log(view.frame());
          result = await receipt;
        } else if (command === "retry") result = await view.local.retry();
        else if (command === "canonical") view.local.useCanonical();
        else if (command !== "frame") { console.log("Unknown command"); continue; }
        if (result) console.log("input:", result.status, "command:", result.commandId,
          "code:", "code" in result ? result.code : "OK");
        console.log(view.frame());
      } catch { console.error("Action failed; check sync status and pending input before retrying."); }
    }
  } finally {
    process.off("SIGINT", stop);
    process.off("SIGTERM", stop);
    terminal?.close();
    view?.dispose();
    await session.leave();
  }
}

main().catch(() => {
  console.error("Connection setup failed. Check environment, enabled auth method, permissions and server rule registration.");
  process.exitCode = 1;
});
