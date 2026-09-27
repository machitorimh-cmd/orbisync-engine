import type { OrbiSyncClient } from "@orbisync/client";

export type User = {
  id: string;
  login_id: string;
  display_name: string;
  enabled: boolean;
  revision: number;
};

export type World = {
  id: string;
  name: string;
  status: "active" | "archived";
  revision: number;
};

export type WorldInstance = {
  id: string;
  world_id: string;
  status: "created" | "running" | "stopping" | "stopped";
  revision: number;
};

type Page<T> = {
  items: T[];
  next_cursor?: string | null;
};

type ErrorEnvelope = {
  error: {
    code: string;
    message: string;
    request_id: string;
  };
};

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string,
    readonly requestId: string | null,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export function normalizeServerUrl(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) throw new Error("サーバーURLを入力してください");
  const parsed = new URL(trimmed);
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("サーバーURLは http または https を使用してください");
  }
  parsed.hash = "";
  parsed.search = "";
  return parsed.toString().replace(/\/$/, "");
}

export class ReferenceApi {
  constructor(private readonly client: OrbiSyncClient) {}

  currentUser(): Promise<User> {
    return this.request<User>("/v1/auth/me", { method: "GET" });
  }

  async listWorlds(): Promise<World[]> {
    return (await this.request<Page<World>>("/v1/worlds?limit=100", { method: "GET" })).items;
  }

  createWorld(name: string, capacity: number): Promise<World> {
    return this.request<World>("/v1/worlds", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, description: "Created by reference-web", capacity }),
    });
  }

  async listInstances(): Promise<WorldInstance[]> {
    return (await this.request<Page<WorldInstance>>("/v1/instances?limit=100", { method: "GET" })).items;
  }

  createInstance(worldId: string, capacity: number): Promise<WorldInstance> {
    return this.request<WorldInstance>("/v1/instances", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ world_id: worldId, capacity }),
    });
  }

  startInstance(instanceId: string): Promise<WorldInstance> {
    return this.request<WorldInstance>(`/v1/instances/${encodeURIComponent(instanceId)}/start`, {
      method: "POST",
    });
  }

  async logout(): Promise<void> {
    await this.request<void>("/v1/auth/logout", { method: "POST" });
  }

  private async request<T>(path: string, init: RequestInit): Promise<T> {
    const response = await this.client.fetch(path, init);
    const text = await response.text();
    const body = text ? parseJson(text) : null;
    if (!response.ok) {
      if (isErrorEnvelope(body)) {
        throw new ApiError(body.error.message, response.status, body.error.code, body.error.request_id);
      }
      throw new ApiError(
        `HTTP ${response.status} ${response.statusText}`.trim(),
        response.status,
        `HTTP_${response.status}`,
        response.headers.get("x-request-id"),
      );
    }
    return body as T;
  }
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

function isErrorEnvelope(value: unknown): value is ErrorEnvelope {
  if (!value || typeof value !== "object" || !("error" in value)) return false;
  const error = (value as { error?: unknown }).error;
  return Boolean(
    error &&
      typeof error === "object" &&
      "code" in error &&
      "message" in error &&
      "request_id" in error,
  );
}
