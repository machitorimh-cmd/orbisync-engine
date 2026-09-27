import { expect, test, type Page, type Route } from "@playwright/test";

const ADMIN_ID = "018f0000-0000-7000-8000-000000000001";
const ADMIN_ROLE_ID = "018f0000-0000-7000-8000-000000000011";
const WORLD_ROLE_ID = "018f0000-0000-7000-8000-000000000012";

type User = {
  id: string;
  login_id: string;
  display_name: string;
  enabled: boolean;
  revision: number;
};

type Role = {
  id: string;
  name: string;
  description: string | null;
  permissions: string[];
  revision: number;
};

type World = { id: string; name: string; status: "active" | "archived"; revision: number };
type Instance = {
  id: string;
  world_id: string;
  status: "created" | "running" | "stopping" | "stopped";
  revision: number;
};

function responseHeaders(): Record<string, string> {
  return {
    "Access-Control-Allow-Origin": "http://127.0.0.1:4174",
    "Access-Control-Allow-Headers": "authorization,content-type,idempotency-key,if-match,accept",
    "Access-Control-Allow-Methods": "GET,POST,PUT,PATCH,DELETE,OPTIONS",
    Vary: "Origin",
  };
}

async function json(route: Route, status: number, body: unknown): Promise<void> {
  await route.fulfill({
    status,
    contentType: "application/json",
    headers: responseHeaders(),
    body: JSON.stringify(body),
  });
}

async function noContent(route: Route): Promise<void> {
  await route.fulfill({ status: 204, headers: responseHeaders() });
}

function page<T>(items: T[]): { items: T[]; next_cursor: null } {
  return { items, next_cursor: null };
}

async function installMockApi(browserPage: Page): Promise<void> {
  const users: User[] = [
    { id: ADMIN_ID, login_id: "admin", display_name: "Administrator", enabled: true, revision: 1 },
  ];
  const roles: Role[] = [
    {
      id: ADMIN_ROLE_ID,
      name: "Administrator",
      description: "Administrative access",
      permissions: ["admin.users.read", "admin.users.create", "admin.users.status", "admin.roles.read", "admin.roles.create", "admin.roles.assign", "admin.worlds.read", "admin.worlds.create", "admin.audit.read"],
      revision: 1,
    },
    {
      id: WORLD_ROLE_ID,
      name: "World Administrator",
      description: "Runtime access",
      permissions: ["world.instance.read", "world.instance.create", "world.instance.start", "world.instance.stop", "moderation.kick"],
      revision: 1,
    },
  ];
  const worlds: World[] = [];
  const instances: Instance[] = [];
  const assignments = new Map<string, string[]>([[ADMIN_ID, [ADMIN_ROLE_ID, WORLD_ROLE_ID]]]);
  const audits: Array<Record<string, unknown>> = [];
  let sequence = 100;

  const nextId = (): string => {
    sequence += 1;
    return `018f0000-0000-7000-8000-${String(sequence).padStart(12, "0")}`;
  };
  const addAudit = (action: string, resourceType: string, resourceId: string | null): void => {
    audits.unshift({
      audit_id: nextId(),
      timestamp: new Date("2026-09-22T12:00:00Z").toISOString(),
      actor_type: "user",
      actor_id: ADMIN_ID,
      action,
      resource_type: resourceType,
      resource_id: resourceId,
      result: "success",
      error_code: null,
      request_id: `req_${nextId()}`,
      details: {},
    });
  };

  await browserPage.route("http://127.0.0.1:8080/**", async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    const method = request.method();

    if (method === "OPTIONS") {
      await noContent(route);
      return;
    }
    if (path === "/health/live") {
      await json(route, 200, { status: "live", failures: [] });
      return;
    }
    if (path === "/health/ready") {
      await json(route, 200, { status: "ready", failures: [] });
      return;
    }
    if (path === "/version") {
      await json(route, 200, { service: "orbisync", version: "test", protocol_major: 1 });
      return;
    }
    if (path === "/v1/auth/login" && method === "POST") {
      expect(request.postDataJSON()).toEqual({ login_id: "admin", password: "correct horse battery staple" });
      await json(route, 200, {
        access_token: "access-token",
        refresh_token: "refresh-token",
        token_type: "Bearer",
        expires_in: 900,
      });
      return;
    }

    expect(request.headers().authorization).toBe("Bearer access-token");

    if (path === "/v1/auth/me" && method === "GET") {
      await json(route, 200, users[0]);
      return;
    }
    if (path === "/v1/auth/logout" && method === "POST") {
      await noContent(route);
      return;
    }
    if (path === "/v1/users" && method === "GET") {
      await json(route, 200, page(users));
      return;
    }
    if (path === "/v1/users" && method === "POST") {
      expect(request.headers().accept).toBe("application/vnd.orbisync.user-credential+json");
      const input = request.postDataJSON() as { login_id: string; display_name: string };
      const user: User = { id: nextId(), ...input, enabled: true, revision: 1 };
      users.push(user);
      assignments.set(user.id, []);
      addAudit("user.created", "user", user.id);
      await json(route, 201, { user, temporary_password: "Temporary-password-000001!" });
      return;
    }
    const userResource = path.match(/^\/v1\/users\/([^/]+)$/);
    if (userResource && method === "PATCH") {
      const user = users.find((item) => item.id === userResource[1]);
      expect(user).toBeTruthy();
      expect(request.headers()["if-match"]).toBe(`"${user!.revision}"`);
      expect(request.headers()["content-type"]).toContain("application/merge-patch+json");
      const input = request.postDataJSON() as { display_name: string };
      user!.display_name = input.display_name;
      user!.revision += 1;
      addAudit("user.updated", "user", user!.id);
      await json(route, 200, user);
      return;
    }
    const userStatus = path.match(/^\/v1\/users\/([^/]+)\/(enable|disable)$/);
    if (userStatus && method === "POST") {
      expect(request.headers()["idempotency-key"]).toMatch(
        /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i,
      );
      const user = users.find((item) => item.id === userStatus[1]);
      expect(user).toBeTruthy();
      user!.enabled = userStatus[2] === "enable";
      user!.revision += 1;
      addAudit(user!.enabled ? "user.enabled" : "user.disabled", "user", user!.id);
      await json(route, 200, user);
      return;
    }
    const userRoles = path.match(/^\/v1\/users\/([^/]+)\/roles$/);
    if (userRoles && method === "GET") {
      await json(route, 200, { user_id: userRoles[1], role_ids: assignments.get(userRoles[1]!) ?? [] });
      return;
    }
    if (userRoles && method === "PUT") {
      const input = request.postDataJSON() as { role_ids: string[] };
      assignments.set(userRoles[1]!, input.role_ids);
      addAudit("role.assigned", "user", userRoles[1]!);
      await json(route, 200, { user_id: userRoles[1], role_ids: input.role_ids });
      return;
    }
    if (path === "/v1/roles" && method === "GET") {
      await json(route, 200, page(roles));
      return;
    }
    if (path === "/v1/roles" && method === "POST") {
      const input = request.postDataJSON() as Omit<Role, "id" | "revision">;
      const role: Role = { id: nextId(), ...input, revision: 1 };
      roles.push(role);
      addAudit("role.created", "role", role.id);
      await json(route, 201, role);
      return;
    }
    const roleResource = path.match(/^\/v1\/roles\/([^/]+)$/);
    if (roleResource && method === "PATCH") {
      const role = roles.find((item) => item.id === roleResource[1]);
      expect(role).toBeTruthy();
      expect(request.headers()["if-match"]).toBe(`"${role!.revision}"`);
      expect(request.headers()["content-type"]).toContain("application/merge-patch+json");
      const input = request.postDataJSON() as Pick<Role, "name" | "description" | "permissions">;
      role!.name = input.name;
      role!.description = input.description;
      role!.permissions = input.permissions;
      role!.revision += 1;
      addAudit("role.updated", "role", role!.id);
      await json(route, 200, role);
      return;
    }
    if (roleResource && method === "DELETE") {
      const index = roles.findIndex((item) => item.id === roleResource[1]);
      expect(index).toBeGreaterThanOrEqual(0);
      expect(request.headers()["if-match"]).toBe(`"${roles[index]!.revision}"`);
      expect([...assignments.values()].some((roleIds) => roleIds.includes(roleResource[1]!))).toBe(false);
      const [deleted] = roles.splice(index, 1);
      addAudit("role.deleted", "role", deleted!.id);
      await noContent(route);
      return;
    }
    if (path === "/v1/worlds" && method === "GET") {
      await json(route, 200, page(worlds));
      return;
    }
    if (path === "/v1/worlds" && method === "POST") {
      const input = request.postDataJSON() as { name: string };
      const world: World = { id: nextId(), name: input.name, status: "active", revision: 1 };
      worlds.push(world);
      addAudit("world.created", "world", world.id);
      await json(route, 201, world);
      return;
    }
    if (path === "/v1/instances" && method === "GET") {
      await json(route, 200, page(instances));
      return;
    }
    if (path === "/v1/instances" && method === "POST") {
      const input = request.postDataJSON() as { world_id: string };
      const instance: Instance = { id: nextId(), world_id: input.world_id, status: "created", revision: 1 };
      instances.push(instance);
      addAudit("world.instance.created", "instance", instance.id);
      await json(route, 201, instance);
      return;
    }
    const instanceStart = path.match(/^\/v1\/instances\/([^/]+)\/start$/);
    if (instanceStart && method === "POST") {
      const instance = instances.find((item) => item.id === instanceStart[1]);
      expect(instance).toBeTruthy();
      instance!.status = "running";
      instance!.revision += 1;
      addAudit("instance.started", "instance", instance!.id);
      await json(route, 202, instance);
      return;
    }
    const instanceMembers = path.match(/^\/v1\/instances\/([^/]+)\/members$/);
    if (instanceMembers && method === "GET") {
      await json(route, 200, page([]));
      return;
    }
    if (path === "/v1/audit-events" && method === "GET") {
      const action = url.searchParams.get("action");
      await json(route, 200, page(action ? audits.filter((item) => item.action === action) : audits));
      return;
    }

    await json(route, 404, {
      error: { code: "RESOURCE_NOT_FOUND", message: `${method} ${path}`, request_id: "req_not_found" },
    });
  });
}

test("管理者の主要操作をブラウザから完了できる", async ({ page: browserPage }) => {
  const browserErrors: string[] = [];
  browserPage.on("pageerror", (error) => browserErrors.push(error.message));
  browserPage.on("console", (message) => {
    if (message.type() === "error") browserErrors.push(message.text());
  });
  await installMockApi(browserPage);

  await browserPage.goto("/");
  await expect(browserPage.locator("#connectionStatus")).toContainText("接続済み");
  await expect(browserPage.locator("#liveStatus")).toHaveText("LIVE");
  await expect(browserPage.locator("#readyStatus")).toHaveText("READY");
  await expect(browserPage.locator("#versionStatus")).toHaveText("test");

  await browserPage.locator("#loginId").fill("admin");
  await browserPage.locator("#password").fill("correct horse battery staple");
  await browserPage.getByRole("button", { name: "管理者としてログイン" }).click();
  await expect(browserPage.locator("#authDetail")).toHaveText("admin / Administrator");
  await expect(browserPage.locator("#userCount")).toHaveText("1 users");
  await expect(browserPage.locator("#roleCount")).toHaveText("2 roles");

  await browserPage.locator("#newLoginId").fill("smoke-player");
  await browserPage.locator("#newDisplayName").fill("Smoke Player");
  await browserPage.getByRole("button", { name: "ユーザー作成" }).click();
  await expect(browserPage.locator("#credentialPanel")).toBeVisible();
  await expect(browserPage.locator("#temporaryPassword")).toHaveText("Temporary-password-000001!");
  await browserPage.getByRole("button", { name: "一時パスワードを画面から消去" }).click();
  await expect(browserPage.locator("#credentialPanel")).toBeHidden();

  const userRow = browserPage.locator("#userTbody tr").filter({ hasText: "smoke-player" });
  await userRow.getByRole("button", { name: "表示名編集" }).click();
  const editUserDialog = browserPage.locator("#editUserDialog");
  await editUserDialog.locator("#editUserDisplayName").fill("Renamed Player");
  await editUserDialog.getByRole("button", { name: "保存" }).click();
  await expect(editUserDialog).not.toBeVisible();
  await expect(userRow).toContainText("Renamed Player");

  browserPage.once("dialog", (dialog) => dialog.accept());
  await userRow.getByRole("button", { name: "無効化" }).click();
  await expect(userRow).toContainText("DISABLED");

  await browserPage.locator("#newRoleName").fill("smoke-observer");
  await browserPage.locator("#newRoleDescription").fill("Read-only administration");
  await browserPage.locator("#newRolePermissions").fill("admin.users.read,admin.worlds.read");
  await browserPage.getByRole("button", { name: "ロール作成" }).click();
  await expect(browserPage.locator("#roleTbody")).toContainText("smoke-observer");

  const createdRoleRow = browserPage.locator("#roleTbody tr").filter({ hasText: "smoke-observer" });
  await createdRoleRow.getByRole("button", { name: "編集" }).click();
  const editRoleDialog = browserPage.locator("#editRoleDialog");
  await editRoleDialog.locator("#editRoleName").fill("smoke-auditor");
  await editRoleDialog.locator("#editRoleDescription").fill("Audit-only administration");
  await editRoleDialog.locator("#editRolePermissions").fill("admin.audit.read\nadmin.users.read");
  await editRoleDialog.getByRole("button", { name: "保存" }).click();
  await expect(editRoleDialog).not.toBeVisible();
  const editedRoleRow = browserPage.locator("#roleTbody tr").filter({ hasText: "smoke-auditor" });
  await expect(editedRoleRow).toContainText("Audit-only administration");
  await expect(editedRoleRow).toContainText("admin.audit.read");

  await userRow.getByRole("button", { name: "ロール" }).click();
  const roleDialog = browserPage.locator("#roleDialog");
  await roleDialog.getByRole("checkbox", { name: /smoke-auditor/ }).check();
  await roleDialog.getByRole("button", { name: "保存" }).click();
  await expect(roleDialog).not.toBeVisible();

  await userRow.getByRole("button", { name: "ロール" }).click();
  await roleDialog.getByRole("checkbox", { name: /smoke-auditor/ }).uncheck();
  await roleDialog.getByRole("button", { name: "保存" }).click();
  await expect(roleDialog).not.toBeVisible();

  browserPage.once("dialog", (dialog) => dialog.accept());
  await editedRoleRow.getByRole("button", { name: "削除" }).click();
  await expect(browserPage.locator("#roleTbody")).not.toContainText("smoke-auditor");

  await browserPage.locator("#newWorldName").fill("Smoke World");
  await browserPage.locator("#newWorldDescription").fill("Browser regression world");
  await browserPage.locator("#newWorldCapacity").fill("32");
  await browserPage.getByRole("button", { name: "World作成" }).click();
  await expect(browserPage.locator("#worldTbody")).toContainText("Smoke World");

  await browserPage.locator("#instanceWorldId").selectOption({ index: 1 });
  await browserPage.locator("#newInstanceCapacity").fill("16");
  await browserPage.getByRole("button", { name: "Instance作成" }).click();
  const instanceRow = browserPage.locator("#instanceTbody tr").filter({ hasText: "Smoke World" });
  await expect(instanceRow).toContainText("created");
  await instanceRow.getByRole("button", { name: "Start" }).click();
  await expect(instanceRow).toContainText("running");
  await instanceRow.getByRole("button", { name: "Members" }).click();
  await expect(browserPage.locator("#memberTbody")).toContainText("現在の参加メンバーはいません");
  await browserPage.locator("#membersDialog").getByRole("button", { name: "閉じる" }).click();

  await browserPage.getByRole("button", { name: "再読み込み" }).last().click();
  await browserPage.locator("#auditActionFilter").fill("instance.started");
  await browserPage.getByRole("button", { name: "絞り込む" }).click();
  await expect(browserPage.locator("#auditTbody tr")).toHaveCount(1);
  await expect(browserPage.locator("#auditTbody")).toContainText("instance.started");
  expect(browserErrors).toEqual([]);
});
