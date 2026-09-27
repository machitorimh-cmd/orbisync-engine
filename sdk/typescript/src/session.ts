import { OrbiSyncClient, type OrbiSyncInstance } from "./client.js";

export type Authentication =
  | { method: "local"; loginId: string; password: string }
  | { method: "guest" }
  | { method: "name_only"; displayName: string }
  | { method: "external"; token: string };

/** Application starter: tokens, tickets, wire state and reconnect remain SDK-owned. */
export async function connectSession(options: {
  baseUrl: string;
  instanceId: string;
  authentication: Authentication;
  onState?: (instance: OrbiSyncInstance) => void;
}) {
  const url = new URL(options.baseUrl);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) {
    throw new Error("Use an HTTP(S) server URL without embedded credentials");
  }
  const client = new OrbiSyncClient({ baseUrl: url.toString().replace(/\/$/, "") });
  const auth = options.authentication;
  if (!(await client.auth.methods()).includes(auth.method)) {
    throw new Error(`Authentication method ${auth.method} is not enabled`);
  }
  switch (auth.method) {
    case "local": await client.auth.login(auth); break;
    case "guest": await client.auth.guest(); break;
    case "name_only": await client.auth.nameOnly(auth); break;
    case "external": await client.auth.external(auth); break;
  }
  const connection = await client.connect();
  let instance: OrbiSyncInstance | undefined;
  const events = ["snapshot", "entitySpawned", "entityUpdated", "entityDeleted", "syncStateChanged", "error"];
  const reflect = () => { if (instance) options.onState?.(instance); };
  const detach = () => { for (const event of events) instance?.off(event, reflect); };
  try {
    instance = await connection.join(options.instanceId);
    for (const event of events) instance.on(event, reflect);
    await instance.ready();
    reflect();
    const joined = instance;
    return {
      instance: joined,
      connection,
      async leave() { detach(); await joined.leave(); },
    };
  } catch (error) {
    detach();
    await connection.disconnect();
    throw error;
  }
}
