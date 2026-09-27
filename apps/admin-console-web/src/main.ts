import {
  ApiError,
  OrbiSyncAdminApi,
  type AuditEvent,
  type Health,
  type InstanceMember,
  type ProbeResult,
  type Role,
  type User,
  type Version,
  type World,
  type WorldInstance,
  normalizeServerUrl,
} from "./api.js";

const DEFAULT_SERVER_URL = "http://127.0.0.1:8080";
const SERVER_URL_STORAGE_KEY = "orbisync.admin.serverUrl";

function byId<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`missing element #${id}`);
  return element as T;
}

function initialServerUrl(): string {
  const saved = localStorage.getItem(SERVER_URL_STORAGE_KEY);
  if (!saved) return DEFAULT_SERVER_URL;
  try {
    return normalizeServerUrl(saved);
  } catch {
    localStorage.removeItem(SERVER_URL_STORAGE_KEY);
    return DEFAULT_SERVER_URL;
  }
}

const serverUrlEl = byId<HTMLInputElement>("serverUrl");
serverUrlEl.value = initialServerUrl();
const api = new OrbiSyncAdminApi(serverUrlEl.value);

let signedInUser: User | null = null;
let users: User[] = [];
let roles: Role[] = [];
let worlds: World[] = [];
let instances: WorldInstance[] = [];
let auditEvents: AuditEvent[] = [];
let nextUsersCursor: string | null = null;
let nextRolesCursor: string | null = null;
let nextWorldsCursor: string | null = null;
let nextInstancesCursor: string | null = null;
let nextAuditCursor: string | null = null;
let roleEditorUser: User | null = null;
let userToEdit: User | null = null;
let roleToEdit: Role | null = null;
let membersInstance: WorldInstance | null = null;

const protectedArea = byId<HTMLDivElement>("protectedArea");
const loginIdEl = byId<HTMLInputElement>("loginId");
const passwordEl = byId<HTMLInputElement>("password");
const loginBtn = byId<HTMLButtonElement>("loginBtn");
const logoutBtn = byId<HTMLButtonElement>("logoutBtn");
const userTbody = byId<HTMLTableSectionElement>("userTbody");
const roleTbody = byId<HTMLTableSectionElement>("roleTbody");
const worldTbody = byId<HTMLTableSectionElement>("worldTbody");
const instanceTbody = byId<HTMLTableSectionElement>("instanceTbody");
const auditTbody = byId<HTMLTableSectionElement>("auditTbody");
const memberTbody = byId<HTMLTableSectionElement>("memberTbody");
const roleDialog = byId<HTMLDialogElement>("roleDialog");
const editUserDialog = byId<HTMLDialogElement>("editUserDialog");
const editRoleDialog = byId<HTMLDialogElement>("editRoleDialog");
const membersDialog = byId<HTMLDialogElement>("membersDialog");

function syncServerUrl(): string {
  const normalized = normalizeServerUrl(serverUrlEl.value);
  api.setBaseUrl(normalized);
  serverUrlEl.value = normalized;
  localStorage.setItem(SERVER_URL_STORAGE_KEY, normalized);
  return normalized;
}

function setConnection(state: "unknown" | "online" | "error", label: string): void {
  const element = byId<HTMLDivElement>("connectionStatus");
  element.dataset.state = state;
  const labelElement = element.querySelector("span:last-child");
  if (labelElement) labelElement.textContent = label;
}

function setAuthenticated(user: User | null): void {
  signedInUser = user;
  const authenticated = user !== null;
  protectedArea.hidden = !authenticated;
  logoutBtn.hidden = !authenticated;
  loginBtn.hidden = authenticated;
  loginIdEl.disabled = authenticated;
  passwordEl.disabled = authenticated;
  serverUrlEl.disabled = authenticated;

  byId<HTMLElement>("authStatus").textContent = authenticated ? "ONLINE" : "OFFLINE";
  byId<HTMLElement>("authDetail").textContent = authenticated
    ? `${user.login_id} / ${user.display_name}`
    : "管理者ログインが必要です";
  byId<HTMLElement>("sessionUser").textContent = authenticated ? user.login_id : "未ログイン";
  byId<HTMLElement>("sessionDetail").textContent = authenticated
    ? `${user.display_name} | tokenはメモリ内のみ`
    : "認証情報はブラウザに保存しません";

  if (!authenticated) {
    clearProtectedState();
    clearCredential();
  }
}

function clearProtectedState(): void {
  users = [];
  roles = [];
  worlds = [];
  instances = [];
  auditEvents = [];
  nextUsersCursor = null;
  nextRolesCursor = null;
  nextWorldsCursor = null;
  nextInstancesCursor = null;
  nextAuditCursor = null;
  renderUsers();
  renderRoles();
  renderWorlds();
  renderInstances();
  renderAudit();
}

function describeError(error: unknown): string {
  if (error instanceof ApiError) {
    const request = error.requestId ? ` / request ${error.requestId}` : "";
    return `${error.code}: ${error.message}${request}`;
  }
  if (error instanceof Error) return error.message;
  return String(error);
}

function handleError(error: unknown, context: string): void {
  if (error instanceof ApiError && error.status === 401 && !api.isAuthenticated()) {
    setAuthenticated(null);
  }
  const message = `${context}: ${describeError(error)}`;
  activity("error", message);
  toast(message, "error");
}

async function withBusy(button: HTMLButtonElement, task: () => Promise<void>): Promise<void> {
  const label = button.textContent;
  button.disabled = true;
  button.setAttribute("aria-busy", "true");
  try {
    await task();
  } finally {
    button.disabled = false;
    button.removeAttribute("aria-busy");
    button.textContent = label;
  }
}

function activity(level: "info" | "success" | "error", message: string): void {
  const list = byId<HTMLOListElement>("activityLog");
  const item = document.createElement("li");
  item.dataset.level = level;
  const time = document.createElement("span");
  time.textContent = new Intl.DateTimeFormat("ja-JP", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date());
  const badge = document.createElement("span");
  badge.textContent = level.toUpperCase();
  const text = document.createElement("span");
  text.textContent = message;
  item.append(time, badge, text);
  list.appendChild(item);
  while (list.children.length > 100) list.firstElementChild?.remove();
  list.scrollTop = list.scrollHeight;
}

function toast(message: string, level: "success" | "error" = "success"): void {
  const region = byId<HTMLDivElement>("toastRegion");
  const item = document.createElement("div");
  item.className = `toast ${level}`;
  item.textContent = message;
  region.appendChild(item);
  window.setTimeout(() => item.remove(), 5_000);
}

function statusBadge(label: string, variant: "success" | "warning" | "error"): HTMLSpanElement {
  const badge = document.createElement("span");
  badge.className = `badge ${variant}`;
  badge.textContent = label;
  return badge;
}

function textCell(value: string, className?: string): HTMLTableCellElement {
  const cell = document.createElement("td");
  cell.textContent = value;
  if (className) cell.className = className;
  return cell;
}

function emptyRow(body: HTMLTableSectionElement, columns: number, message: string): void {
  body.replaceChildren();
  const row = document.createElement("tr");
  const cell = textCell(message, "empty-cell");
  cell.colSpan = columns;
  row.appendChild(cell);
  body.appendChild(row);
}

function actionButton(
  label: string,
  style: "secondary" | "danger-ghost",
  action: (button: HTMLButtonElement) => Promise<void>,
): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = `button small ${style}`;
  button.textContent = label;
  button.addEventListener("click", () => {
    void withBusy(button, () => action(button)).catch((error) => handleError(error, label));
  });
  return button;
}

function shortId(value: string): string {
  return value.length > 13 ? `${value.slice(0, 8)}…${value.slice(-4)}` : value;
}

function mergeById<T extends { id: string }>(current: T[], incoming: T[]): T[] {
  const merged = new Map(current.map((item) => [item.id, item]));
  for (const item of incoming) merged.set(item.id, item);
  return [...merged.values()];
}

function probeFailures(result: ProbeResult<Health>): string {
  const body = result.body;
  if (!body || typeof body !== "object") return `HTTP ${result.status}`;
  if ("failures" in body && Array.isArray(body.failures) && body.failures.length > 0) {
    return body.failures.join(", ");
  }
  if ("error" in body) {
    const error = (body as { error?: { message?: unknown; details?: { failures?: unknown } } }).error;
    if (Array.isArray(error?.details?.failures) && error.details.failures.length > 0) {
      return error.details.failures.join(", ");
    }
    if (typeof error?.message === "string") return error.message;
  }
  return result.ok ? "正常" : `HTTP ${result.status}`;
}

async function refreshOverview(): Promise<void> {
  syncServerUrl();
  setConnection("unknown", "確認中…");
  const [liveResult, readyResult, versionResult] = await Promise.allSettled([
    api.probe<Health>("/health/live"),
    api.probe<Health>("/health/ready"),
    api.probe<Version>("/version"),
  ]);

  if (liveResult.status === "fulfilled") {
    byId<HTMLElement>("liveStatus").textContent = liveResult.value.ok ? "LIVE" : "ERROR";
    byId<HTMLElement>("liveDetail").textContent = probeFailures(liveResult.value);
  } else {
    byId<HTMLElement>("liveStatus").textContent = "OFFLINE";
    byId<HTMLElement>("liveDetail").textContent = describeError(liveResult.reason);
  }
  if (readyResult.status === "fulfilled") {
    byId<HTMLElement>("readyStatus").textContent = readyResult.value.ok ? "READY" : "DEGRADED";
    byId<HTMLElement>("readyDetail").textContent = probeFailures(readyResult.value);
  } else {
    byId<HTMLElement>("readyStatus").textContent = "OFFLINE";
    byId<HTMLElement>("readyDetail").textContent = describeError(readyResult.reason);
  }
  if (versionResult.status === "fulfilled" && versionResult.value.ok) {
    const version = versionResult.value.body as Version;
    byId<HTMLElement>("versionStatus").textContent = version.version;
    byId<HTMLElement>("protocolStatus").textContent = `protocol ${version.protocol_major}`;
  } else {
    byId<HTMLElement>("versionStatus").textContent = "—";
    byId<HTMLElement>("protocolStatus").textContent = "取得できません";
  }

  if (liveResult.status === "fulfilled" && liveResult.value.ok) {
    setConnection("online", readyResult.status === "fulfilled" && readyResult.value.ok ? "接続済み" : "接続済み・準備未完了");
  } else {
    setConnection("error", "接続できません");
    throw liveResult.status === "rejected"
      ? liveResult.reason
      : new Error(`liveness check failed: ${probeFailures(liveResult.value)}`);
  }
}

async function loadProtectedData(): Promise<void> {
  const tasks: Array<[string, Promise<void>]> = [
    ["ユーザー一覧", loadUsers(true)],
    ["ロール一覧", loadRoles(true)],
    ["World一覧", loadWorlds(true)],
    ["Instance一覧", loadInstances(true)],
    ["監査ログ", loadAudit(true)],
  ];
  const results = await Promise.allSettled(tasks.map(([, task]) => task));
  results.forEach((result, index) => {
    if (result.status === "rejected") {
      activity("error", `${tasks[index]![0]}を取得できません: ${describeError(result.reason)}`);
    }
  });
}

async function loadUsers(reset: boolean): Promise<void> {
  const page = await api.listUsers(reset ? null : nextUsersCursor);
  users = reset ? page.items : mergeById(users, page.items);
  nextUsersCursor = page.next_cursor ?? null;
  renderUsers();
}

function renderUsers(): void {
  if (users.length === 0) {
    emptyRow(userTbody, 6, signedInUser ? "ユーザーがありません" : "ログイン後に表示されます");
  } else {
    userTbody.replaceChildren();
    for (const user of users) {
      const row = document.createElement("tr");
      row.append(textCell(user.login_id), textCell(user.display_name));
      const status = document.createElement("td");
      status.appendChild(statusBadge(user.enabled ? "ACTIVE" : "DISABLED", user.enabled ? "success" : "error"));
      row.append(status, textCell(String(user.revision)));
      const id = textCell(shortId(user.id), "id-text");
      id.title = user.id;
      row.appendChild(id);
      const actions = document.createElement("td");
      actions.className = "actions";
      actions.append(
        actionButton("表示名編集", "secondary", async () => openUserEditor(user)),
        actionButton(user.enabled ? "無効化" : "有効化", user.enabled ? "danger-ghost" : "secondary", async () => {
          const verb = user.enabled ? "無効化" : "有効化";
          if (!window.confirm(`${user.login_id} を${verb}しますか？`)) return;
          const updated = await api.setUserEnabled(user.id, !user.enabled);
          users = users.map((item) => (item.id === updated.id ? updated : item));
          renderUsers();
          activity("success", `${updated.login_id} を${verb}しました`);
        }),
        actionButton("PW再発行", "secondary", async () => {
          if (!window.confirm(`${user.login_id} の既存セッションを失効させ、一時パスワードを発行しますか？`)) return;
          const result = await api.resetUserPassword(user.id);
          if (!result.temporary_password) throw new Error("一時パスワードが応答に含まれていません");
          showCredential(user.login_id, result.temporary_password);
          activity("success", `${user.login_id} の一時パスワードを発行しました`);
        }),
        actionButton("ロール", "secondary", async () => openRoleEditor(user)),
      );
      row.appendChild(actions);
      userTbody.appendChild(row);
    }
  }
  byId<HTMLElement>("userCount").textContent = `${users.length} users`;
  byId<HTMLButtonElement>("nextUsersBtn").hidden = !nextUsersCursor;
}

function showCredential(loginId: string, password: string): void {
  byId<HTMLElement>("credentialUser").textContent = loginId;
  byId<HTMLElement>("temporaryPassword").textContent = password;
  byId<HTMLElement>("credentialPanel").hidden = false;
}

function clearCredential(): void {
  byId<HTMLElement>("credentialUser").textContent = "—";
  byId<HTMLElement>("temporaryPassword").textContent = "";
  byId<HTMLElement>("credentialPanel").hidden = true;
}

function openUserEditor(user: User): void {
  userToEdit = user;
  byId<HTMLElement>("editUserDialogTitle").textContent = `${user.login_id} の表示名`;
  const input = byId<HTMLInputElement>("editUserDisplayName");
  input.value = user.display_name;
  editUserDialog.showModal();
  input.focus();
  input.select();
}

async function loadRoles(reset: boolean): Promise<void> {
  const page = await api.listRoles(reset ? null : nextRolesCursor);
  roles = reset ? page.items : mergeById(roles, page.items);
  nextRolesCursor = page.next_cursor ?? null;
  renderRoles();
}

async function loadAllRoles(): Promise<void> {
  let collected: Role[] = [];
  let cursor: string | null = null;
  const seenCursors = new Set<string>();
  do {
    const page = await api.listRoles(cursor);
    collected = mergeById(collected, page.items);
    cursor = page.next_cursor ?? null;
    if (cursor && seenCursors.has(cursor)) throw new Error("ロール一覧のpagination cursorが循環しました");
    if (cursor) seenCursors.add(cursor);
  } while (cursor);
  roles = collected;
  nextRolesCursor = null;
  renderRoles();
}

function renderRoles(): void {
  if (roles.length === 0) {
    emptyRow(roleTbody, 6, signedInUser ? "ロールがありません" : "ログイン後に表示されます");
  } else {
    roleTbody.replaceChildren();
    for (const role of roles) {
      const row = document.createElement("tr");
      row.append(textCell(role.name), textCell(role.description ?? "—"));
      const permissions = textCell(role.permissions.join(", ") || "—", "permission-list");
      permissions.title = role.permissions.join("\n");
      row.append(permissions, textCell(String(role.revision)));
      const id = textCell(shortId(role.id), "id-text");
      id.title = role.id;
      row.appendChild(id);
      const actions = document.createElement("td");
      actions.className = "actions";
      actions.append(
        actionButton("編集", "secondary", async () => openRoleDefinitionEditor(role)),
        actionButton("削除", "danger-ghost", async () => {
          if (!window.confirm(`${role.name} を削除しますか？割り当て済みの場合は削除できません。`)) return;
          await api.deleteRole(role.id, role.revision);
          roles = roles.filter((item) => item.id !== role.id);
          renderRoles();
          activity("success", `${role.name} ロールを削除しました`);
        }),
      );
      row.appendChild(actions);
      roleTbody.appendChild(row);
    }
  }
  byId<HTMLElement>("roleCount").textContent = `${roles.length} roles`;
  byId<HTMLButtonElement>("nextRolesBtn").hidden = !nextRolesCursor;
}

function openRoleDefinitionEditor(role: Role): void {
  roleToEdit = role;
  byId<HTMLElement>("editRoleDialogTitle").textContent = `${role.name} を編集`;
  byId<HTMLInputElement>("editRoleName").value = role.name;
  byId<HTMLInputElement>("editRoleDescription").value = role.description ?? "";
  byId<HTMLTextAreaElement>("editRolePermissions").value = role.permissions.join("\n");
  editRoleDialog.showModal();
  byId<HTMLInputElement>("editRoleName").focus();
}

async function openRoleEditor(user: User): Promise<void> {
  roleEditorUser = user;
  // PUT replaces the complete assignment, so every role must be visible before
  // saving. Loading only the first page could accidentally remove hidden roles.
  await loadAllRoles();
  const assignment = await api.getUserRoles(user.id);
  const selected = new Set(assignment.role_ids);
  byId<HTMLElement>("roleDialogTitle").textContent = `${user.login_id} のロール`;
  const container = byId<HTMLDivElement>("roleCheckboxes");
  container.replaceChildren();
  if (roles.length === 0) {
    const empty = document.createElement("p");
    empty.textContent = "割り当て可能なロールがありません。先にロールを作成してください。";
    container.appendChild(empty);
  }
  for (const role of roles) {
    const label = document.createElement("label");
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = role.id;
    checkbox.checked = selected.has(role.id);
    const description = document.createElement("span");
    description.textContent = role.name;
    const detail = document.createElement("small");
    detail.textContent = role.permissions.join(", ") || "権限なし";
    description.appendChild(detail);
    label.append(checkbox, description);
    container.appendChild(label);
  }
  roleDialog.showModal();
}

async function loadWorlds(reset: boolean): Promise<void> {
  const page = await api.listWorlds(reset ? null : nextWorldsCursor);
  worlds = reset ? page.items : mergeById(worlds, page.items);
  nextWorldsCursor = page.next_cursor ?? null;
  renderWorlds();
  renderWorldSelect();
}

function renderWorlds(): void {
  if (worlds.length === 0) {
    emptyRow(worldTbody, 5, signedInUser ? "Worldがありません" : "ログイン後に表示されます");
  } else {
    worldTbody.replaceChildren();
    for (const world of worlds) {
      const row = document.createElement("tr");
      row.appendChild(textCell(world.name));
      const status = document.createElement("td");
      status.appendChild(statusBadge(world.status, world.status === "active" ? "success" : "warning"));
      row.append(status, textCell(String(world.revision)));
      const id = textCell(shortId(world.id), "id-text");
      id.title = world.id;
      row.appendChild(id);
      const actions = document.createElement("td");
      actions.className = "actions";
      if (world.status === "active") {
        actions.appendChild(actionButton("Archive", "danger-ghost", async () => {
          if (!window.confirm(`${world.name} をarchiveしますか？この操作は元に戻せません。`)) return;
          const updated = await api.archiveWorld(world.id);
          worlds = worlds.map((item) => (item.id === updated.id ? updated : item));
          renderWorlds();
          renderWorldSelect();
          activity("success", `${world.name} をarchiveしました`);
        }));
      } else {
        actions.textContent = "—";
      }
      row.appendChild(actions);
      worldTbody.appendChild(row);
    }
  }
  byId<HTMLElement>("worldCount").textContent = `${worlds.length} worlds`;
  byId<HTMLButtonElement>("nextWorldsBtn").hidden = !nextWorldsCursor;
}

function renderWorldSelect(): void {
  const select = byId<HTMLSelectElement>("instanceWorldId");
  const previous = select.value;
  select.replaceChildren();
  const placeholder = document.createElement("option");
  placeholder.value = "";
  placeholder.textContent = "Worldを選択";
  select.appendChild(placeholder);
  for (const world of worlds.filter((item) => item.status === "active")) {
    const option = document.createElement("option");
    option.value = world.id;
    option.textContent = `${world.name} | ${shortId(world.id)}`;
    select.appendChild(option);
  }
  if (Array.from(select.options).some((option) => option.value === previous)) select.value = previous;
}

async function loadInstances(reset: boolean): Promise<void> {
  const page = await api.listInstances(reset ? null : nextInstancesCursor);
  instances = reset ? page.items : mergeById(instances, page.items);
  nextInstancesCursor = page.next_cursor ?? null;
  renderInstances();
}

function renderInstances(): void {
  if (instances.length === 0) {
    emptyRow(instanceTbody, 5, signedInUser ? "Instanceがありません" : "ログイン後に表示されます");
  } else {
    instanceTbody.replaceChildren();
    for (const instance of instances) {
      const row = document.createElement("tr");
      const id = textCell(shortId(instance.id), "id-text");
      id.title = instance.id;
      const world = worlds.find((item) => item.id === instance.world_id);
      const worldCell = textCell(world?.name ?? shortId(instance.world_id));
      worldCell.title = instance.world_id;
      const statusCell = document.createElement("td");
      const variant = instance.status === "running" ? "success" : instance.status === "stopping" ? "warning" : "error";
      statusCell.appendChild(statusBadge(instance.status, variant));
      row.append(id, worldCell, statusCell, textCell(String(instance.revision)));
      const actions = document.createElement("td");
      actions.className = "actions";
      if (instance.status === "created" || instance.status === "stopped") {
        actions.appendChild(actionButton("Start", "secondary", async () => updateInstanceState(instance, true)));
      } else if (instance.status === "running") {
        actions.appendChild(actionButton("Stop", "danger-ghost", async () => {
          if (!window.confirm(`${shortId(instance.id)} を停止しますか？`)) return;
          await updateInstanceState(instance, false);
        }));
      }
      actions.appendChild(actionButton("Members", "secondary", async () => openMembers(instance)));
      row.appendChild(actions);
      instanceTbody.appendChild(row);
    }
  }
  byId<HTMLElement>("instanceCount").textContent = `${instances.length} instances`;
  byId<HTMLButtonElement>("nextInstancesBtn").hidden = !nextInstancesCursor;
}

async function updateInstanceState(instance: WorldInstance, running: boolean): Promise<void> {
  const updated = await api.setInstanceRunning(instance.id, running);
  instances = instances.map((item) => (item.id === updated.id ? updated : item));
  renderInstances();
  activity("success", `${shortId(instance.id)} の${running ? "起動" : "停止"}を受け付けました`);
}

async function openMembers(instance: WorldInstance): Promise<void> {
  membersInstance = instance;
  byId<HTMLElement>("membersDialogTitle").textContent = `${shortId(instance.id)} の参加メンバー`;
  renderMembers(await loadAllInstanceMembers(instance.id));
  membersDialog.showModal();
}

async function loadAllInstanceMembers(instanceId: string): Promise<InstanceMember[]> {
  let members: InstanceMember[] = [];
  let cursor: string | null = null;
  const seenCursors = new Set<string>();
  do {
    const page = await api.listInstanceMembers(instanceId, cursor);
    members = [...members, ...page.items];
    cursor = page.next_cursor ?? null;
    if (cursor && seenCursors.has(cursor)) throw new Error("参加者一覧のpagination cursorが循環しました");
    if (cursor) seenCursors.add(cursor);
  } while (cursor);
  return members;
}

function renderMembers(members: InstanceMember[]): void {
  if (members.length === 0) {
    emptyRow(memberTbody, 2, "現在の参加メンバーはいません");
    return;
  }
  memberTbody.replaceChildren();
  for (const member of members) {
    const row = document.createElement("tr");
    const id = textCell(member.user_id, "id-text");
    const actions = document.createElement("td");
    actions.appendChild(actionButton("Kick", "danger-ghost", async () => {
      if (!membersInstance) return;
      if (!window.confirm(`${member.user_id} をInstanceからkickしますか？`)) return;
      await api.kickInstanceMember(membersInstance.id, member.user_id);
      renderMembers(await loadAllInstanceMembers(membersInstance.id));
      activity("success", `${shortId(member.user_id)} をkickしました`);
    }));
    row.append(id, actions);
    memberTbody.appendChild(row);
  }
}

async function loadAudit(reset: boolean): Promise<void> {
  const action = byId<HTMLInputElement>("auditActionFilter").value.trim();
  const page = await api.listAuditEvents(reset ? null : nextAuditCursor, action || undefined);
  auditEvents = reset ? page.items : mergeAudit(auditEvents, page.items);
  nextAuditCursor = page.next_cursor ?? null;
  renderAudit();
}

function mergeAudit(current: AuditEvent[], incoming: AuditEvent[]): AuditEvent[] {
  const merged = new Map(current.map((item) => [item.audit_id, item]));
  for (const item of incoming) merged.set(item.audit_id, item);
  return [...merged.values()];
}

function renderAudit(): void {
  if (auditEvents.length === 0) {
    emptyRow(auditTbody, 6, signedInUser ? "監査イベントがありません" : "ログイン後に表示されます");
  } else {
    auditTbody.replaceChildren();
    for (const event of auditEvents) {
      const row = document.createElement("tr");
      row.appendChild(textCell(formatDate(event.timestamp)));
      const result = document.createElement("td");
      result.appendChild(statusBadge(event.result, event.result === "success" ? "success" : "error"));
      const action = textCell(event.action);
      action.title = JSON.stringify(event.details);
      const actor = textCell(event.actor_id ? `${event.actor_type} | ${shortId(event.actor_id)}` : event.actor_type);
      const resource = textCell(event.resource_id ? `${event.resource_type} | ${shortId(event.resource_id)}` : event.resource_type);
      const request = textCell(shortId(event.request_id), "id-text");
      request.title = event.request_id;
      row.append(result, action, actor, resource, request);
      auditTbody.appendChild(row);
    }
  }
  byId<HTMLElement>("auditCount").textContent = `${auditEvents.length} events`;
  byId<HTMLButtonElement>("nextAuditBtn").hidden = !nextAuditCursor;
}

function formatDate(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat("ja-JP", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  }).format(date);
}

function parsePermissions(value: string): string[] {
  return [...new Set(value.split(/[\n,]/).map((item) => item.trim()).filter(Boolean))];
}

function validatedRolePermissions(value: string): string[] {
  const permissions = parsePermissions(value);
  if (permissions.length === 0) throw new Error("権限を1つ以上入力してください");
  const hasAdminPermission = permissions.some((permission) => permission.startsWith("admin."));
  const hasWorldPermission = permissions.some((permission) => !permission.startsWith("admin."));
  if (hasAdminPermission && hasWorldPermission) {
    throw new Error("admin系権限とWorld系権限は同じロールへ混在できません");
  }
  return permissions;
}

function requiredInteger(input: HTMLInputElement, label: string): number {
  const value = Number(input.value);
  if (!Number.isInteger(value) || value < 1 || value > 1000) {
    throw new Error(`${label}は1から1000の整数で入力してください`);
  }
  return value;
}

byId<HTMLButtonElement>("checkServerBtn").addEventListener("click", (event) => {
  const button = event.currentTarget as HTMLButtonElement;
  void withBusy(button, async () => {
    await refreshOverview();
    activity("success", `${api.getBaseUrl()} へ接続しました`);
  }).catch((error) => handleError(error, "接続確認"));
});

byId<HTMLButtonElement>("refreshOverviewBtn").addEventListener("click", (event) => {
  const button = event.currentTarget as HTMLButtonElement;
  void withBusy(button, refreshOverview).catch((error) => handleError(error, "状態取得"));
});

byId<HTMLFormElement>("loginForm").addEventListener("submit", (event) => {
  event.preventDefault();
  void withBusy(loginBtn, async () => {
    syncServerUrl();
    const password = passwordEl.value;
    passwordEl.value = "";
    const user = await api.login(loginIdEl.value.trim(), password);
    setAuthenticated(user);
    activity("success", `${user.login_id} としてログインしました`);
    toast("ログインしました");
    await refreshOverview();
    await loadProtectedData();
  }).catch((error) => handleError(error, "ログイン"));
});

logoutBtn.addEventListener("click", () => {
  void withBusy(logoutBtn, async () => {
    try {
      await api.logout();
    } finally {
      setAuthenticated(null);
    }
    activity("success", "ログアウトしました");
  }).catch((error) => handleError(error, "ログアウト"));
});

byId<HTMLFormElement>("createUserForm").addEventListener("submit", (event) => {
  event.preventDefault();
  const button = byId<HTMLButtonElement>("createUserBtn");
  void withBusy(button, async () => {
    const loginId = byId<HTMLInputElement>("newLoginId").value.trim();
    const displayName = byId<HTMLInputElement>("newDisplayName").value.trim();
    const result = await api.createUser(loginId, displayName);
    users = mergeById(users, [result.user]);
    renderUsers();
    showCredential(result.user.login_id, result.temporary_password);
    byId<HTMLFormElement>("createUserForm").reset();
    activity("success", `${result.user.login_id} を作成しました`);
  }).catch((error) => handleError(error, "ユーザー作成"));
});

byId<HTMLFormElement>("editUserForm").addEventListener("submit", (event) => {
  event.preventDefault();
  const button = byId<HTMLButtonElement>("saveEditUserBtn");
  void withBusy(button, async () => {
    const user = userToEdit;
    if (!user) return;
    const displayName = byId<HTMLInputElement>("editUserDisplayName").value.trim();
    if (!displayName) throw new Error("表示名を入力してください");
    const updated = await api.updateUserDisplayName(user.id, user.revision, displayName);
    users = users.map((item) => (item.id === updated.id ? updated : item));
    renderUsers();
    if (signedInUser?.id === updated.id) setAuthenticated(updated);
    editUserDialog.close();
    userToEdit = null;
    activity("success", `${updated.login_id} の表示名を更新しました`);
  }).catch((error) => handleError(error, "表示名更新"));
});

for (const id of ["closeEditUserBtn", "cancelEditUserBtn"]) {
  byId<HTMLButtonElement>(id).addEventListener("click", () => {
    editUserDialog.close();
    userToEdit = null;
  });
}

byId<HTMLButtonElement>("copyPasswordBtn").addEventListener("click", () => {
  const password = byId<HTMLElement>("temporaryPassword").textContent ?? "";
  if (!password) return;
  void navigator.clipboard.writeText(password)
    .then(() => toast("一時パスワードをコピーしました"))
    .catch((error) => handleError(error, "コピー"));
});
byId<HTMLButtonElement>("clearPasswordBtn").addEventListener("click", clearCredential);

byId<HTMLFormElement>("createRoleForm").addEventListener("submit", (event) => {
  event.preventDefault();
  const button = byId<HTMLButtonElement>("createRoleBtn");
  void withBusy(button, async () => {
    const name = byId<HTMLInputElement>("newRoleName").value.trim();
    const description = byId<HTMLInputElement>("newRoleDescription").value.trim() || null;
    const permissions = validatedRolePermissions(byId<HTMLTextAreaElement>("newRolePermissions").value);
    const role = await api.createRole(name, description, permissions);
    roles = mergeById(roles, [role]);
    renderRoles();
    byId<HTMLFormElement>("createRoleForm").reset();
    activity("success", `${role.name} ロールを作成しました`);
  }).catch((error) => handleError(error, "ロール作成"));
});

byId<HTMLFormElement>("editRoleForm").addEventListener("submit", (event) => {
  event.preventDefault();
  const button = byId<HTMLButtonElement>("saveEditRoleBtn");
  void withBusy(button, async () => {
    const role = roleToEdit;
    if (!role) return;
    const name = byId<HTMLInputElement>("editRoleName").value.trim();
    const description = byId<HTMLInputElement>("editRoleDescription").value.trim() || null;
    const permissions = validatedRolePermissions(byId<HTMLTextAreaElement>("editRolePermissions").value);
    const updated = await api.updateRole(role.id, role.revision, name, description, permissions);
    roles = roles.map((item) => (item.id === updated.id ? updated : item));
    renderRoles();
    editRoleDialog.close();
    roleToEdit = null;
    activity("success", `${updated.name} ロールを更新しました`);
  }).catch((error) => handleError(error, "ロール更新"));
});

for (const id of ["closeEditRoleBtn", "cancelEditRoleBtn"]) {
  byId<HTMLButtonElement>(id).addEventListener("click", () => {
    editRoleDialog.close();
    roleToEdit = null;
  });
}

byId<HTMLButtonElement>("saveUserRolesBtn").addEventListener("click", (event) => {
  const button = event.currentTarget as HTMLButtonElement;
  void withBusy(button, async () => {
    const user = roleEditorUser;
    if (!user) return;
    const checked = Array.from(
      byId<HTMLDivElement>("roleCheckboxes").querySelectorAll<HTMLInputElement>('input[type="checkbox"]:checked'),
    );
    const roleIds = checked.map((input) => input.value);
    if (signedInUser?.id === user.id && !window.confirm("自分自身のロールを変更します。管理権限を失う可能性があります。続行しますか？")) return;
    await api.replaceUserRoles(user.id, roleIds);
    roleDialog.close();
    activity("success", `${user.login_id} のロールを更新しました`);
  }).catch((error) => handleError(error, "ロール割り当て"));
});

byId<HTMLFormElement>("createWorldForm").addEventListener("submit", (event) => {
  event.preventDefault();
  const button = byId<HTMLButtonElement>("createWorldBtn");
  void withBusy(button, async () => {
    const name = byId<HTMLInputElement>("newWorldName").value.trim();
    const description = byId<HTMLInputElement>("newWorldDescription").value.trim() || null;
    const capacity = requiredInteger(byId<HTMLInputElement>("newWorldCapacity"), "最大人数");
    const world = await api.createWorld(name, description, capacity);
    worlds = mergeById(worlds, [world]);
    renderWorlds();
    renderWorldSelect();
    byId<HTMLFormElement>("createWorldForm").reset();
    byId<HTMLInputElement>("newWorldCapacity").value = "100";
    activity("success", `${world.name} を作成しました`);
  }).catch((error) => handleError(error, "World作成"));
});

byId<HTMLFormElement>("createInstanceForm").addEventListener("submit", (event) => {
  event.preventDefault();
  const button = byId<HTMLButtonElement>("createInstanceBtn");
  void withBusy(button, async () => {
    const worldId = byId<HTMLSelectElement>("instanceWorldId").value;
    if (!worldId) throw new Error("Worldを選択してください");
    const capacityInput = byId<HTMLInputElement>("newInstanceCapacity");
    const capacity = capacityInput.value.trim() ? requiredInteger(capacityInput, "最大人数") : null;
    const instance = await api.createInstance(worldId, capacity);
    instances = mergeById(instances, [instance]);
    renderInstances();
    capacityInput.value = "";
    activity("success", `${shortId(instance.id)} を作成しました`);
  }).catch((error) => handleError(error, "Instance作成"));
});

const refreshBindings: Array<[string, (reset: boolean) => Promise<void>, string]> = [
  ["refreshUsersBtn", loadUsers, "ユーザー一覧"], ["refreshRolesBtn", loadRoles, "ロール一覧"],
  ["refreshWorldsBtn", loadWorlds, "World一覧"], ["refreshInstancesBtn", loadInstances, "Instance一覧"],
  ["refreshAuditBtn", loadAudit, "監査ログ"],
];
for (const [id, loader, label] of refreshBindings) {
  byId<HTMLButtonElement>(id).addEventListener("click", (event) => {
    const button = event.currentTarget as HTMLButtonElement;
    void withBusy(button, () => loader(true)).catch((error) => handleError(error, label));
  });
}

const nextBindings: Array<[string, (reset: boolean) => Promise<void>, string]> = [
  ["nextUsersBtn", loadUsers, "ユーザー追加取得"], ["nextRolesBtn", loadRoles, "ロール追加取得"],
  ["nextWorldsBtn", loadWorlds, "World追加取得"], ["nextInstancesBtn", loadInstances, "Instance追加取得"],
  ["nextAuditBtn", loadAudit, "監査ログ追加取得"],
];
for (const [id, loader, label] of nextBindings) {
  byId<HTMLButtonElement>(id).addEventListener("click", (event) => {
    const button = event.currentTarget as HTMLButtonElement;
    void withBusy(button, () => loader(false)).catch((error) => handleError(error, label));
  });
}

byId<HTMLButtonElement>("applyAuditFilterBtn").addEventListener("click", (event) => {
  const button = event.currentTarget as HTMLButtonElement;
  void withBusy(button, () => loadAudit(true)).catch((error) => handleError(error, "監査ログ絞り込み"));
});
byId<HTMLButtonElement>("clearLogBtn").addEventListener("click", () => byId<HTMLOListElement>("activityLog").replaceChildren());

serverUrlEl.addEventListener("change", () => {
  try {
    syncServerUrl();
    setConnection("unknown", "未確認");
  } catch (error) {
    handleError(error, "サーバーURL");
  }
});

renderUsers();
renderRoles();
renderWorlds();
renderInstances();
renderAudit();
setAuthenticated(null);
activity("info", "OrbiSync Controlを起動しました。認証情報は永続化しません。" );
void refreshOverview().catch((error) => activity("error", `初回接続確認: ${describeError(error)}`));

window.setInterval(() => {
  if (document.visibilityState === "visible") {
    void refreshOverview().catch(() => undefined);
  }
}, 30_000);
