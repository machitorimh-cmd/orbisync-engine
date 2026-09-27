import {
  OrbiSyncClient,
  createUuidV7,
  type ConnectionPhase,
  type ConnectionState,
  type EntityCommand,
  type EntityState,
  type ErrorMessage,
  type OrbiSyncConnection,
  type OrbiSyncInstance,
} from "@orbisync/client";
import {
  ApiError,
  ReferenceApi,
  normalizeServerUrl,
  type User,
  type World,
  type WorldInstance,
} from "./api.js";
import "./styles.css";

type LogLevel = "info" | "success" | "error";
type TrackedEntity = {
  id: string;
  revision: bigint;
  x: number;
  y: number;
  label: string;
  ownerId: string | null;
};

function element<T extends HTMLElement>(id: string): T {
  const found = document.getElementById(id);
  if (!found) throw new Error(`missing element #${id}`);
  return found as T;
}

const serverUrlEl = element<HTMLInputElement>("serverUrl");
const loginIdEl = element<HTMLInputElement>("loginId");
const passwordEl = element<HTMLInputElement>("password");
const loginBtn = element<HTMLButtonElement>("loginBtn");
const logoutBtn = element<HTMLButtonElement>("logoutBtn");
const authStatusEl = element<HTMLSpanElement>("authStatus");
const worldSelect = element<HTMLSelectElement>("worldSelect");
const worldNameEl = element<HTMLInputElement>("worldName");
const worldCapacityEl = element<HTMLInputElement>("worldCapacity");
const refreshBtn = element<HTMLButtonElement>("refreshBtn");
const createWorldBtn = element<HTMLButtonElement>("createWorldBtn");
const instanceSelect = element<HTMLSelectElement>("instanceSelect");
const instanceCapacityEl = element<HTMLInputElement>("instanceCapacity");
const createInstanceBtn = element<HTMLButtonElement>("createInstanceBtn");
const startInstanceBtn = element<HTMLButtonElement>("startInstanceBtn");
const instanceStatusEl = element<HTMLElement>("instanceStatus");
const joinBtn = element<HTMLButtonElement>("joinBtn");
const reconnectBtn = element<HTMLButtonElement>("reconnectBtn");
const disconnectBtn = element<HTMLButtonElement>("disconnectBtn");
const phaseValueEl = element<HTMLElement>("phaseValue");
const attemptValueEl = element<HTMLElement>("attemptValue");
const rttValueEl = element<HTMLElement>("rttValue");
const revisionValueEl = element<HTMLElement>("revisionValue");
const queueValueEl = element<HTMLElement>("queueValue");
const joinInfoEl = element<HTMLElement>("joinInfo");
const entitySelect = element<HTMLSelectElement>("entitySelect");
const visibilitySelect = element<HTMLSelectElement>("visibilitySelect");
const propertyValueEl = element<HTMLInputElement>("propertyValue");
const newOwnerIdEl = element<HTMLInputElement>("newOwnerId");
const spawnBtn = element<HTMLButtonElement>("spawnBtn");
const updateBtn = element<HTMLButtonElement>("updateBtn");
const deleteBtn = element<HTMLButtonElement>("deleteBtn");
const transferBtn = element<HTMLButtonElement>("transferBtn");
const entityStatusEl = element<HTMLElement>("entityStatus");
const overallStatusEl = element<HTMLElement>("overallStatus");
const overallDotEl = element<HTMLElement>("overallDot");
const logEl = element<HTMLOListElement>("log");
const clearLogBtn = element<HTMLButtonElement>("clearLogBtn");
const canvas = element<HTMLCanvasElement>("canvas");
const context = canvas.getContext("2d");
if (!context) throw new Error("2D canvas is unavailable");
const drawContext: CanvasRenderingContext2D = context;

let client: OrbiSyncClient | null = null;
let api: ReferenceApi | null = null;
let currentUser: User | null = null;
let worlds: World[] = [];
let instances: WorldInstance[] = [];
let connection: OrbiSyncConnection | null = null;
let instance: OrbiSyncInstance | null = null;
let unsubscribeConnectionState: (() => void) | null = null;
let lastPhase: ConnectionPhase = "closed";
let recoveryRequested = false;
let transformPending = false;
let transformTimer: ReturnType<typeof setTimeout> | null = null;
let previousPosition: { id: string; x: number; y: number } | null = null;
const entities = new Map<string, TrackedEntity>();

const savedServer = localStorage.getItem("orbisync.reference.serverUrl");
if (savedServer) serverUrlEl.value = savedServer;
worldNameEl.value = `reference-${Date.now().toString(36)}`;

function log(message: string, level: LogLevel = "info"): void {
  const item = document.createElement("li");
  item.dataset.level = level;
  const time = document.createElement("time");
  time.dateTime = new Date().toISOString();
  time.textContent = new Date().toLocaleTimeString("ja-JP", { hour12: false });
  item.append(time, document.createTextNode(message));
  logEl.prepend(item);
  while (logEl.children.length > 100) logEl.lastElementChild?.remove();
}

function describeError(error: unknown): string {
  if (error instanceof ApiError) {
    const requestId = error.requestId ? ` request_id=${error.requestId}` : "";
    return `${error.code} (HTTP ${error.status}): ${error.message}${requestId}`;
  }
  return error instanceof Error ? error.message : String(error);
}

async function runAction(button: HTMLButtonElement, action: () => Promise<void>): Promise<void> {
  button.disabled = true;
  try {
    await action();
  } catch (error) {
    log(describeError(error), "error");
    overallStatusEl.textContent = "操作エラー";
    overallDotEl.dataset.state = "error";
  } finally {
    syncControls();
  }
}

function selectedWorld(): World | null {
  return worlds.find((world) => world.id === worldSelect.value) ?? null;
}

function selectedInstance(): WorldInstance | null {
  return instances.find((candidate) => candidate.id === instanceSelect.value) ?? null;
}

function parseCapacity(input: HTMLInputElement): number {
  const value = Number(input.value);
  if (!Number.isInteger(value) || value < 1 || value > 1000) {
    throw new Error("定員は1〜1000の整数で入力してください");
  }
  return value;
}

function syncControls(): void {
  const authenticated = client !== null && api !== null && currentUser !== null;
  const world = selectedWorld();
  const selected = selectedInstance();
  const phase = connection?.getConnectionState().phase ?? "closed";
  const joined = instance !== null && phase === "connected";
  const entitySelected = joined && entities.has(entitySelect.value);

  logoutBtn.disabled = !authenticated;
  refreshBtn.disabled = !authenticated;
  createWorldBtn.disabled = !authenticated;
  worldSelect.disabled = !authenticated || worlds.length === 0;
  createInstanceBtn.disabled = !authenticated || world === null;
  instanceSelect.disabled = !authenticated || world === null || filteredInstances().length === 0;
  startInstanceBtn.disabled = !authenticated || selected === null || selected.status === "running";
  joinBtn.disabled = !authenticated || selected?.status !== "running" || connection !== null;
  reconnectBtn.disabled = !joined;
  disconnectBtn.disabled = connection === null;
  spawnBtn.disabled = !joined;
  entitySelect.disabled = !joined || entities.size === 0;
  updateBtn.disabled = !entitySelected;
  deleteBtn.disabled = !entitySelected;
  transferBtn.disabled = !entitySelected || newOwnerIdEl.value.trim().length === 0;
  instanceStatusEl.textContent = selected?.status ?? "—";
}

function filteredInstances(): WorldInstance[] {
  return instances.filter((candidate) => candidate.world_id === worldSelect.value);
}

function renderWorlds(preferredId?: string): void {
  const previous = preferredId ?? worldSelect.value;
  worldSelect.replaceChildren();
  for (const world of worlds) {
    const option = document.createElement("option");
    option.value = world.id;
    option.textContent = `${world.name} | ${world.status}`;
    worldSelect.append(option);
  }
  if (worlds.some((world) => world.id === previous)) worldSelect.value = previous;
  renderInstances();
}

function renderInstances(preferredId?: string): void {
  const previous = preferredId ?? instanceSelect.value;
  const visible = filteredInstances();
  instanceSelect.replaceChildren();
  if (visible.length === 0) {
    const option = document.createElement("option");
    option.value = "";
    option.textContent = selectedWorld() ? "Instanceはありません" : "Worldを選択してください";
    instanceSelect.append(option);
  } else {
    for (const candidate of visible) {
      const option = document.createElement("option");
      option.value = candidate.id;
      option.textContent = `${candidate.id.slice(0, 13)}… | ${candidate.status}`;
      instanceSelect.append(option);
    }
    if (visible.some((candidate) => candidate.id === previous)) instanceSelect.value = previous;
  }
  syncControls();
}

async function refreshResources(preferredWorldId?: string, preferredInstanceId?: string): Promise<void> {
  if (!api) throw new Error("先にログインしてください");
  [worlds, instances] = await Promise.all([api.listWorlds(), api.listInstances()]);
  renderWorlds(preferredWorldId);
  if (preferredInstanceId) renderInstances(preferredInstanceId);
  log(`World ${worlds.length}件 / Instance ${instances.length}件を取得`, "success");
}

async function disconnectRealtime(): Promise<void> {
  unsubscribeConnectionState?.();
  unsubscribeConnectionState = null;
  if (connection) await connection.disconnect();
  connection = null;
  instance = null;
  transformPending = false;
  if (transformTimer) clearTimeout(transformTimer);
  transformTimer = null;
  phaseValueEl.textContent = "closed";
  attemptValueEl.textContent = "0";
  rttValueEl.textContent = "—";
  queueValueEl.textContent = "0";
  joinInfoEl.textContent = "切断しました。再参加には「接続して参加」を押してください。";
  overallStatusEl.textContent = currentUser ? "認証済み" : "未接続";
  overallDotEl.dataset.state = currentUser ? "success" : "";
  syncControls();
}

function handleConnectionState(state: ConnectionState): void {
  phaseValueEl.textContent = state.phase;
  attemptValueEl.textContent = String(state.reconnectAttempt);
  rttValueEl.textContent = state.rttMs === null ? "—" : `${state.rttMs} ms`;
  revisionValueEl.textContent = state.lastAppliedRevision.toString();
  if (state.phase !== lastPhase) {
    log(`接続状態: ${lastPhase} → ${state.phase}${state.lastDisconnect?.reason ? ` (${state.lastDisconnect.reason})` : ""}`);
    lastPhase = state.phase;
  }
  if (state.phase === "connected") {
    overallStatusEl.textContent = recoveryRequested ? "自動復帰済み" : "Realtime接続中";
    overallDotEl.dataset.state = "success";
    if (recoveryRequested) {
      log("resume経路でRealtime接続が復帰しました", "success");
      recoveryRequested = false;
    }
  } else if (state.phase === "reconnecting" || state.phase === "resyncing") {
    overallStatusEl.textContent = state.phase === "reconnecting" ? "再接続中" : "再同期中";
    overallDotEl.dataset.state = "warning";
  } else if (state.phase === "offline") {
    overallStatusEl.textContent = "オフライン";
    overallDotEl.dataset.state = "error";
  }
  syncControls();
}

function isEntityCommand(value: unknown): value is EntityCommand {
  return Boolean(value && typeof value === "object" && "operation" in value && "entityId" in value);
}

function isEntityState(value: unknown): value is EntityState {
  return Boolean(value && typeof value === "object" && "entityId" in value && "revision" in value && !("operation" in value));
}

function commandArgument(command: EntityCommand, key: string): unknown {
  return (command.arguments as Record<string, unknown> | undefined)?.[key];
}

function handleEntityUpdate(value: unknown): void {
  if (isEntityCommand(value)) {
    const existing = entities.get(value.entityId);
    if (value.operation === "delete") {
      entities.delete(value.entityId);
      log(`Entity削除を確認: ${value.entityId}`, "success");
    } else {
      const x = Number(commandArgument(value, "position_x") ?? existing?.x ?? 50);
      const y = Number(commandArgument(value, "position_y") ?? existing?.y ?? 50);
      const nextOwner = commandArgument(value, "new_owner_id");
      const label = commandArgument(value, "value");
      entities.set(value.entityId, {
        id: value.entityId,
        revision: value.expectedRevision,
        x: Number.isFinite(x) ? x : 50,
        y: Number.isFinite(y) ? y : 50,
        label: typeof label === "string" ? label : existing?.label ?? "entity",
        ownerId: typeof nextOwner === "string" ? nextOwner : existing?.ownerId ?? currentUser?.id ?? null,
      });
      log(`${value.operation} 適用: ${value.entityId} revision=${value.expectedRevision}`, "success");
    }
    finishPendingTransform(value.entityId);
  } else if (isEntityState(value)) {
    const existing = entities.get(value.entityId);
    entities.set(value.entityId, {
      id: value.entityId,
      revision: value.revision,
      x: value.transform?.positionX ?? existing?.x ?? 50,
      y: value.transform?.positionY ?? existing?.y ?? 50,
      label: existing?.label ?? "entity",
      ownerId: existing?.ownerId ?? null,
    });
    finishPendingTransform(value.entityId);
  }
  renderEntities();
}

function handleRealtimeError(value: unknown): void {
  const error = value as Partial<ErrorMessage>;
  log(`Realtime error ${error.code ?? "UNKNOWN"}: ${error.message ?? "no detail"}`, "error");
  if (previousPosition) {
    const tracked = entities.get(previousPosition.id);
    if (tracked) {
      tracked.x = previousPosition.x;
      tracked.y = previousPosition.y;
    }
  }
  finishPendingTransform();
  renderEntities();
}

function finishPendingTransform(entityId?: string): void {
  if (entityId && previousPosition?.id !== entityId) return;
  transformPending = false;
  previousPosition = null;
  if (transformTimer) clearTimeout(transformTimer);
  transformTimer = null;
}

function renderEntities(preferredId?: string): void {
  const previous = preferredId ?? entitySelect.value;
  entitySelect.replaceChildren();
  for (const tracked of entities.values()) {
    const option = document.createElement("option");
    option.value = tracked.id;
    option.textContent = `${tracked.label} | r${tracked.revision} | ${tracked.id.slice(0, 8)}…`;
    entitySelect.append(option);
  }
  if (entities.has(previous)) entitySelect.value = previous;
  const selected = entities.get(entitySelect.value);
  entityStatusEl.textContent = selected
    ? `id=${selected.id} | revision=${selected.revision} | position=(${selected.x.toFixed(2)}, ${selected.y.toFixed(2)}) | owner=${selected.ownerId ?? "unknown"}`
    : "Entityはまだありません。";
  drawCanvas();
  syncControls();
}

function drawCanvas(): void {
  drawContext.clearRect(0, 0, canvas.width, canvas.height);
  drawContext.strokeStyle = "#162033";
  drawContext.lineWidth = 1;
  for (let x = 0; x <= canvas.width; x += 76) {
    drawContext.beginPath(); drawContext.moveTo(x, 0); drawContext.lineTo(x, canvas.height); drawContext.stroke();
  }
  for (let y = 0; y <= canvas.height; y += 72) {
    drawContext.beginPath(); drawContext.moveTo(0, y); drawContext.lineTo(canvas.width, y); drawContext.stroke();
  }
  for (const tracked of entities.values()) {
    const x = Math.max(18, Math.min(canvas.width - 18, tracked.x / 100 * canvas.width));
    const y = Math.max(18, Math.min(canvas.height - 18, tracked.y / 100 * canvas.height));
    const selected = tracked.id === entitySelect.value;
    drawContext.fillStyle = selected ? "#4f97ff" : "#45cfa9";
    drawContext.beginPath(); drawContext.arc(x, y, selected ? 9 : 7, 0, Math.PI * 2); drawContext.fill();
    drawContext.fillStyle = "#cbd5e5";
    drawContext.font = "12px ui-monospace, monospace";
    drawContext.fillText(`${tracked.label} | r${tracked.revision}`, x + 13, y - 9);
  }
}

function moveSelected(dx: number, dy: number): void {
  if (!instance || transformPending) return;
  const tracked = entities.get(entitySelect.value);
  if (!tracked) return;
  previousPosition = { id: tracked.id, x: tracked.x, y: tracked.y };
  tracked.x = Math.max(0, Math.min(100, tracked.x + dx));
  tracked.y = Math.max(0, Math.min(100, tracked.y + dy));
  transformPending = true;
  instance.sendTransform({ entityId: tracked.id, position: { x: tracked.x, y: tracked.y, z: 0 } });
  transformTimer = setTimeout(() => {
    log("移動応答が2秒以内に届きませんでした。次の入力を許可します。", "error");
    finishPendingTransform();
  }, 2_000);
  drawCanvas();
}

loginBtn.addEventListener("click", () => void runAction(loginBtn, async () => {
  const baseUrl = normalizeServerUrl(serverUrlEl.value);
  const loginId = loginIdEl.value.trim();
  if (!loginId || !passwordEl.value) throw new Error("Login IDとPasswordを入力してください");
  if (connection) await disconnectRealtime();
  const nextClient = new OrbiSyncClient({
    baseUrl,
    clientName: "@orbisync/reference-web",
    clientVersion: "0.1.0",
    clientType: "desktop",
  });
  await nextClient.auth.login({ loginId, password: passwordEl.value });
  const nextApi = new ReferenceApi(nextClient);
  const user = await nextApi.currentUser();
  client = nextClient;
  api = nextApi;
  currentUser = user;
  passwordEl.value = "";
  localStorage.setItem("orbisync.reference.serverUrl", baseUrl);
  authStatusEl.textContent = `${user.display_name} (${user.id})`;
  authStatusEl.dataset.state = "success";
  overallStatusEl.textContent = "認証済み";
  overallDotEl.dataset.state = "success";
  log(`SDK login成功: ${user.login_id} / ${baseUrl}`, "success");
  await refreshResources();
}));

logoutBtn.addEventListener("click", () => void runAction(logoutBtn, async () => {
  if (connection) await disconnectRealtime();
  try {
    await api?.logout();
  } finally {
    client = null;
    api = null;
    currentUser = null;
    worlds = [];
    instances = [];
    entities.clear();
    renderWorlds();
    renderEntities();
    authStatusEl.textContent = "未認証";
    authStatusEl.dataset.state = "";
    overallStatusEl.textContent = "未接続";
    overallDotEl.dataset.state = "";
  }
  log("ログアウトしました", "success");
}));

refreshBtn.addEventListener("click", () => void runAction(refreshBtn, () => refreshResources()));
createWorldBtn.addEventListener("click", () => void runAction(createWorldBtn, async () => {
  if (!api) throw new Error("先にログインしてください");
  const name = worldNameEl.value.trim();
  if (!name) throw new Error("World名を入力してください");
  const created = await api.createWorld(name, parseCapacity(worldCapacityEl));
  log(`World作成: ${created.name} (${created.id})`, "success");
  worldNameEl.value = `reference-${Date.now().toString(36)}`;
  await refreshResources(created.id);
}));

worldSelect.addEventListener("change", () => renderInstances());
instanceSelect.addEventListener("change", syncControls);
createInstanceBtn.addEventListener("click", () => void runAction(createInstanceBtn, async () => {
  if (!api || !selectedWorld()) throw new Error("Worldを選択してください");
  const created = await api.createInstance(worldSelect.value, parseCapacity(instanceCapacityEl));
  log(`Instance作成: ${created.id}`, "success");
  await refreshResources(worldSelect.value, created.id);
}));

startInstanceBtn.addEventListener("click", () => void runAction(startInstanceBtn, async () => {
  if (!api || !selectedInstance()) throw new Error("Instanceを選択してください");
  const started = await api.startInstance(instanceSelect.value);
  log(`Instance起動受付: ${started.id} status=${started.status}`, "success");
  await refreshResources(worldSelect.value, started.id);
}));

joinBtn.addEventListener("click", () => void runAction(joinBtn, async () => {
  if (!client || selectedInstance()?.status !== "running") throw new Error("running状態のInstanceを選択してください");
  const nextConnection = await client.connect();
  connection = nextConnection;
  lastPhase = "connected";
  unsubscribeConnectionState = nextConnection.onConnectionStateChange(handleConnectionState);
  try {
    const joined = await nextConnection.join(instanceSelect.value);
    instance = joined;
    joined.on("entityUpdated", handleEntityUpdate);
    joined.on("error", handleRealtimeError);
    joined.on("snapshot", () => log("Snapshotを受信", "success"));
    joined.on("resumeAccepted", () => log("ResumeAcceptedを受信", "success"));
    joined.on("resyncRequired", () => log("ResyncRequiredを受信。SDKが再joinします。"));
    await joined.ready();
    const info = joined.getJoinInfo();
    joinInfoEl.textContent = `user=${info.userId} | presence=${info.presenceId} | nearby=${info.nearbyEntityCount} | permissions spawn:${info.permissions.entitySpawn} own:${info.permissions.entityUpdateOwn} any:${info.permissions.entityUpdateAny}`;
    log(`Realtime参加成功: instance=${info.instanceId} presence=${info.presenceId}`, "success");
    overallStatusEl.textContent = "Realtime接続中";
    overallDotEl.dataset.state = "success";
  } catch (error) {
    await disconnectRealtime();
    throw error;
  }
}));

reconnectBtn.addEventListener("click", () => {
  try {
    if (!connection) throw new Error("Realtimeへ接続していません");
    recoveryRequested = true;
    connection.requestReconnect();
    log("transportを閉じ、自動resume経路の検証を開始しました");
    syncControls();
  } catch (error) {
    recoveryRequested = false;
    log(describeError(error), "error");
  }
});

disconnectBtn.addEventListener("click", () => void runAction(disconnectBtn, async () => {
  await disconnectRealtime();
  log("Realtimeを明示切断しました", "success");
}));

spawnBtn.addEventListener("click", () => {
  try {
    if (!instance) throw new Error("先にInstanceへ参加してください");
    const id = createUuidV7();
    const x = 20 + Math.random() * 60;
    const y = 20 + Math.random() * 60;
    instance.sendEntityCommand({
      entityId: id,
      operation: "spawn",
      args: { kind: "object", visibility: visibilitySelect.value, position_x: x, position_y: y, position_z: 0 },
    });
    log(`spawn送信: ${id}`);
  } catch (error) {
    log(describeError(error), "error");
  }
});

updateBtn.addEventListener("click", () => {
  try {
    if (!instance || !entities.has(entitySelect.value)) throw new Error("Entityを選択してください");
    instance.sendEntityCommand({
      entityId: entitySelect.value,
      operation: "update",
      args: { component_key: "reference.color", value: propertyValueEl.value.trim() || "unset" },
    });
    log(`update送信: ${entitySelect.value}`);
  } catch (error) {
    log(describeError(error), "error");
  }
});

deleteBtn.addEventListener("click", () => {
  try {
    if (!instance || !entities.has(entitySelect.value)) throw new Error("Entityを選択してください");
    instance.sendEntityCommand({ entityId: entitySelect.value, operation: "delete" });
    log(`delete送信: ${entitySelect.value}`);
  } catch (error) {
    log(describeError(error), "error");
  }
});

transferBtn.addEventListener("click", () => {
  try {
    if (!instance || !entities.has(entitySelect.value)) throw new Error("Entityを選択してください");
    const newOwnerId = newOwnerIdEl.value.trim();
    if (!newOwnerId) throw new Error("移転先User IDを入力してください");
    instance.transferEntityOwnership({ entityId: entitySelect.value, newOwnerId });
    log(`transfer_ownership送信: entity=${entitySelect.value} target=${newOwnerId}`);
  } catch (error) {
    log(describeError(error), "error");
  }
});

newOwnerIdEl.addEventListener("input", syncControls);
entitySelect.addEventListener("change", () => renderEntities());
clearLogBtn.addEventListener("click", () => logEl.replaceChildren());

window.addEventListener("keydown", (event) => {
  if (event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement) return;
  const movement: Record<string, [number, number]> = {
    w: [0, -0.25], arrowup: [0, -0.25], s: [0, 0.25], arrowdown: [0, 0.25],
    a: [-0.25, 0], arrowleft: [-0.25, 0], d: [0.25, 0], arrowright: [0.25, 0],
  };
  const delta = movement[event.key.toLowerCase()];
  if (!delta) return;
  event.preventDefault();
  moveSelected(delta[0], delta[1]);
});

canvas.addEventListener("click", (event) => {
  const rect = canvas.getBoundingClientRect();
  const x = (event.clientX - rect.left) / rect.width * canvas.width;
  const y = (event.clientY - rect.top) / rect.height * canvas.height;
  let nearest: { id: string; distance: number } | null = null;
  for (const tracked of entities.values()) {
    const entityX = tracked.x / 100 * canvas.width;
    const entityY = tracked.y / 100 * canvas.height;
    const distance = Math.hypot(entityX - x, entityY - y);
    if (distance <= 24 && (!nearest || distance < nearest.distance)) nearest = { id: tracked.id, distance };
  }
  if (nearest) renderEntities(nearest.id);
});

setInterval(() => {
  queueValueEl.textContent = String(connection?.getQueueMetrics().reliable.queueLength ?? 0);
}, 250);

renderEntities();
syncControls();
log("reference-webを起動しました。最初にSDKでログインしてください。");
