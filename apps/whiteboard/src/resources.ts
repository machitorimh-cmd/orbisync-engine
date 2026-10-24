import type { OrbiSyncClient } from "../../../sdk/typescript/src/client.js";
import { pagePath, type Page } from "../../shared/cursor_pages.mjs";

export type World = { id: string; name: string; status: string };
export type WorldInstance = { id: string; world_id: string; status: string };

/** Use the SDK refresh lifecycle and retain the opaque cursor for the next click. */
export async function resourcePage<T>(client: Pick<OrbiSyncClient, "fetch">, resource: "worlds" | "instances", cursor: string | null): Promise<Page<T>> {
  const response = await client.fetch(pagePath(resource, cursor));
  if (!response.ok) throw new Error(`${resource} 一覧 HTTP ${response.status}`);
  return await response.json() as Page<T>;
}
