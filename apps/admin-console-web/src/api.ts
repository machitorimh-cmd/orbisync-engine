export type User = {
  id: string;
  login_id: string;
  display_name: string;
  enabled: boolean;
  revision: number;
};

export type Role = {
  id: string;
  name: string;
  description: string | null;
  permissions: string[];
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

export type InstanceMember = {
  user_id: string;
};

export type AuditEvent = {
  audit_id: string;
  timestamp: string;
  actor_type: "user" | "admin" | "system";
  actor_id?: string | null;
  action: string;
  resource_type: string;
  resource_id?: string | null;
  result: "success" | "failure";
  error_code?: string | null;
  request_id: string;
  details: Record<string, unknown>;
};

export type Page<T> = {
  items: T[];
  next_cursor?: string | null;
};

export type Health = {
  status: "live" | "ready";
  failures: string[];
};

export type Version = {
  service: string;
  version: string;
  protocol_major: number;
};

export type ProbeResult<T> = {
  ok: boolean;
  status: number;
  body: T | ErrorEnvelope | null;
};

export type CreatedUserCredential = {
  user: User;
  temporary_password: string;
};

export type TemporaryPassword = {
  temporary_password?: string;
  must_change_password: true;
};

export type UserRoleAssignment = {
  user_id: string;
  role_ids: string[];
};

type LoginResponse = {
  access_token: string;
  refresh_token: string;
  token_type: "Bearer";
  expires_in: number;
};

type ErrorEnvelope = {
  error: {
    code: string;
    message: string;
    request_id: string;
    details?: Record<string, unknown>;
  };
};

type RequestOptions = {
  authenticated?: boolean;
  allowRefresh?: boolean;
};

const REQUEST_TIMEOUT_MS = 15_000;

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

  static async fromResponse(response: Response): Promise<ApiError> {
    const fallbackRequestId = response.headers.get("x-request-id");
    const body = await readBody(response);
    if (isErrorEnvelope(body)) {
      return new ApiError(
        body.error.message,
        response.status,
        body.error.code,
        body.error.request_id || fallbackRequestId,
      );
    }
    return new ApiError(
      `HTTP ${response.status} ${response.statusText}`.trim(),
      response.status,
      `HTTP_${response.status}`,
      fallbackRequestId,
    );
  }
}

export class NetworkError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NetworkError";
  }
}

export function normalizeServerUrl(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) throw new Error("サーバーURLを入力してください");

  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    throw new Error("サーバーURLが正しくありません");
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    throw new Error("サーバーURLはhttpまたはhttpsを使用してください");
  }
  parsed.hash = "";
  parsed.search = "";
  return parsed.toString().replace(/\/$/, "");
}

/** Generates a canonical UUIDv7 for endpoints that require Idempotency-Key. */
export function newIdempotencyKey(): string {
  const bytes = new Uint8Array(16);
  const timestamp = Date.now();
  for (let index = 5; index >= 0; index -= 1) {
    bytes[index] = Math.floor(timestamp / 2 ** (8 * (5 - index))) & 0xff;
  }
  crypto.getRandomValues(bytes.subarray(6));
  bytes[6] = (bytes[6]! & 0x0f) | 0x70;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export class OrbiSyncAdminApi {
  private baseUrl: string;
  private accessToken: string | null = null;
  private refreshToken: string | null = null;
  private refreshInFlight: Promise<void> | null = null;

  constructor(baseUrl: string) {
    this.baseUrl = normalizeServerUrl(baseUrl);
  }

  setBaseUrl(value: string): void {
    const next = normalizeServerUrl(value);
    if (next !== this.baseUrl) this.clearSession();
    this.baseUrl = next;
  }

  getBaseUrl(): string {
    return this.baseUrl;
  }

  isAuthenticated(): boolean {
    return this.accessToken !== null;
  }

  clearSession(): void {
    this.accessToken = null;
    this.refreshToken = null;
  }

  async probe<T>(path: string): Promise<ProbeResult<T>> {
    const response = await this.performFetch(path, { method: "GET" }, false);
    return {
      ok: response.ok,
      status: response.status,
      body: (await readBody(response)) as T | ErrorEnvelope | null,
    };
  }

  async login(loginId: string, password: string): Promise<User> {
    this.clearSession();
    const tokens = await this.request<LoginResponse>(
      "/v1/auth/login",
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ login_id: loginId, password }),
      },
      { authenticated: false, allowRefresh: false },
    );
    this.accessToken = tokens.access_token;
    this.refreshToken = tokens.refresh_token;
    try {
      return await this.currentUser();
    } catch (error) {
      this.clearSession();
      throw error;
    }
  }

  async logout(): Promise<void> {
    try {
      if (this.accessToken) {
        await this.request<void>("/v1/auth/logout", { method: "POST" });
      }
    } finally {
      this.clearSession();
    }
  }

  currentUser(): Promise<User> {
    return this.request<User>("/v1/auth/me", { method: "GET" });
  }

  listUsers(cursor?: string | null): Promise<Page<User>> {
    return this.request<Page<User>>(withPage("/v1/users", cursor), { method: "GET" });
  }

  createUser(loginId: string, displayName: string): Promise<CreatedUserCredential> {
    return this.request<CreatedUserCredential>("/v1/users", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/vnd.orbisync.user-credential+json",
      },
      body: JSON.stringify({ login_id: loginId, display_name: displayName }),
    });
  }

  setUserEnabled(userId: string, enabled: boolean): Promise<User> {
    return this.request<User>(`/v1/users/${encodeURIComponent(userId)}/${enabled ? "enable" : "disable"}`, {
      method: "POST",
      headers: { "Idempotency-Key": newIdempotencyKey() },
    });
  }

  updateUserDisplayName(userId: string, revision: number, displayName: string): Promise<User> {
    return this.request<User>(`/v1/users/${encodeURIComponent(userId)}`, {
      method: "PATCH",
      headers: {
        "Content-Type": "application/merge-patch+json",
        "If-Match": revisionHeader(revision),
      },
      body: JSON.stringify({ display_name: displayName }),
    });
  }

  resetUserPassword(userId: string): Promise<TemporaryPassword> {
    return this.request<TemporaryPassword>(`/v1/users/${encodeURIComponent(userId)}/reset-password`, {
      method: "POST",
      headers: { "Idempotency-Key": newIdempotencyKey() },
    });
  }

  listRoles(cursor?: string | null): Promise<Page<Role>> {
    return this.request<Page<Role>>(withPage("/v1/roles", cursor), { method: "GET" });
  }

  createRole(name: string, description: string | null, permissions: string[]): Promise<Role> {
    return this.request<Role>("/v1/roles", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, description, permissions }),
    });
  }

  updateRole(
    roleId: string,
    revision: number,
    name: string,
    description: string | null,
    permissions: string[],
  ): Promise<Role> {
    return this.request<Role>(`/v1/roles/${encodeURIComponent(roleId)}`, {
      method: "PATCH",
      headers: {
        "Content-Type": "application/merge-patch+json",
        "If-Match": revisionHeader(revision),
      },
      body: JSON.stringify({ name, description, permissions }),
    });
  }

  deleteRole(roleId: string, revision: number): Promise<void> {
    return this.request<void>(`/v1/roles/${encodeURIComponent(roleId)}`, {
      method: "DELETE",
      headers: { "If-Match": revisionHeader(revision) },
    });
  }

  getUserRoles(userId: string): Promise<UserRoleAssignment> {
    return this.request<UserRoleAssignment>(`/v1/users/${encodeURIComponent(userId)}/roles`, {
      method: "GET",
    });
  }

  replaceUserRoles(userId: string, roleIds: string[]): Promise<UserRoleAssignment> {
    return this.request<UserRoleAssignment>(`/v1/users/${encodeURIComponent(userId)}/roles`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ role_ids: roleIds }),
    });
  }

  listWorlds(cursor?: string | null): Promise<Page<World>> {
    return this.request<Page<World>>(withPage("/v1/worlds", cursor), { method: "GET" });
  }

  createWorld(name: string, description: string | null, capacity: number): Promise<World> {
    return this.request<World>("/v1/worlds", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, description, capacity }),
    });
  }

  archiveWorld(worldId: string): Promise<World> {
    return this.request<World>(`/v1/worlds/${encodeURIComponent(worldId)}/archive`, {
      method: "POST",
      headers: { "Idempotency-Key": newIdempotencyKey() },
    });
  }

  listInstances(cursor?: string | null): Promise<Page<WorldInstance>> {
    return this.request<Page<WorldInstance>>(withPage("/v1/instances", cursor), { method: "GET" });
  }

  createInstance(worldId: string, capacity: number | null): Promise<WorldInstance> {
    return this.request<WorldInstance>("/v1/instances", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(capacity === null ? { world_id: worldId } : { world_id: worldId, capacity }),
    });
  }

  setInstanceRunning(instanceId: string, running: boolean): Promise<WorldInstance> {
    return this.request<WorldInstance>(
      `/v1/instances/${encodeURIComponent(instanceId)}/${running ? "start" : "stop"}`,
      { method: "POST" },
    );
  }

  listInstanceMembers(instanceId: string, cursor?: string | null): Promise<Page<InstanceMember>> {
    return this.request<Page<InstanceMember>>(
      withPage(`/v1/instances/${encodeURIComponent(instanceId)}/members`, cursor),
      { method: "GET" },
    );
  }

  kickInstanceMember(instanceId: string, userId: string): Promise<void> {
    return this.request<void>(
      `/v1/instances/${encodeURIComponent(instanceId)}/kick/${encodeURIComponent(userId)}`,
      { method: "POST" },
    );
  }

  listAuditEvents(cursor?: string | null, action?: string): Promise<Page<AuditEvent>> {
    const params = new URLSearchParams({ limit: "100" });
    if (cursor) params.set("cursor", cursor);
    if (action) params.set("action", action);
    return this.request<Page<AuditEvent>>(`/v1/audit-events?${params.toString()}`, { method: "GET" });
  }

  private async request<T>(
    path: string,
    init: RequestInit,
    options: RequestOptions = {},
  ): Promise<T> {
    const authenticated = options.authenticated ?? true;
    const allowRefresh = options.allowRefresh ?? true;
    let response = await this.performFetch(path, init, authenticated);

    if (response.status === 401 && authenticated && allowRefresh && this.refreshToken) {
      await this.refreshSession();
      response = await this.performFetch(path, init, true);
    }

    if (!response.ok) throw await ApiError.fromResponse(response);
    if (response.status === 204) return undefined as T;
    const body = await readBody(response);
    return body as T;
  }

  private async refreshSession(): Promise<void> {
    if (!this.refreshInFlight) {
      this.refreshInFlight = this.performRefresh().finally(() => {
        this.refreshInFlight = null;
      });
    }
    return this.refreshInFlight;
  }

  private async performRefresh(): Promise<void> {
    const token = this.refreshToken;
    if (!token) throw new ApiError("セッションの有効期限が切れました", 401, "AUTHENTICATION_REQUIRED", null);
    try {
      const tokens = await this.request<LoginResponse>(
        "/v1/auth/refresh",
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ refresh_token: token }),
        },
        { authenticated: false, allowRefresh: false },
      );
      this.accessToken = tokens.access_token;
      this.refreshToken = tokens.refresh_token;
    } catch (error) {
      this.clearSession();
      throw error;
    }
  }

  private async performFetch(
    path: string,
    init: RequestInit,
    authenticated: boolean,
  ): Promise<Response> {
    const headers = new Headers(init.headers);
    if (!headers.has("Accept")) headers.set("Accept", "application/json");
    if (authenticated && this.accessToken) headers.set("Authorization", `Bearer ${this.accessToken}`);

    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
    try {
      return await fetch(`${this.baseUrl}${path}`, {
        ...init,
        headers,
        signal: controller.signal,
      });
    } catch (error) {
      if (error instanceof DOMException && error.name === "AbortError") {
        throw new NetworkError("サーバーから15秒以内に応答がありませんでした");
      }
      throw new NetworkError(
        "サーバーへ接続できません。URL、サーバー起動状態、CORS許可設定を確認してください",
      );
    } finally {
      clearTimeout(timer);
    }
  }
}

function withPage(path: string, cursor?: string | null): string {
  const params = new URLSearchParams({ limit: "100" });
  if (cursor) params.set("cursor", cursor);
  return `${path}?${params.toString()}`;
}

function revisionHeader(revision: number): string {
  if (!Number.isSafeInteger(revision) || revision < 1) throw new Error("revisionが正しくありません");
  return `"${revision}"`;
}

async function readBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return null;
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
