/** Privileged, count-only operational diagnostics; no tokens or entity payloads. */
export type DiagnosticsSnapshot = {
  collected_at: string;
  active_connections: number;
  checkpoint: { latest_at: string | null; save_failures_total: number; restore_failures_total: number; save_rejected_too_large_total: number; restore_rejected_too_large_total: number };
  queues: {
    control: { last_observed_depth: number; capacity_per_instance: number };
    transform: { last_observed_depth: number; capacity_per_instance: number };
    entity: { last_observed_depth: number; capacity_per_instance: number };
    outbound: { last_observed_depth: number; capacity_per_connection: number; depth_high_water: number; last_observed_bytes: number; bytes_high_water: number };
  };
  presence: {
    observed_at: string | null; connected: number; grace_waiting: number; reclaim_waiting: number;
    memberships: number; memberships_without_binding: number; bindings_without_membership: number;
    joined_connections: number; connection_ownership_mismatch: number;
  };
  persistence: { pending: number; oldest_pending_seconds: number; entity_events: number; command_outcomes: number; generation_snapshots: number };
  resume_rejections_total: { token_mismatch: number; history_unavailable: number; revision_gap: number; future_revision: number; not_authorized: number; ownership_lost: number };
  extensions: { outbox_pending: number; deliveries_in_flight: number; dead_letters: number };
  rate_limit_rejections_total: { connection: number; user: number; ip: number; password_hash: number; instance: number };
  retention: { realtime_tickets_last_success_at: string | null; idempotency_records_last_success_at: string | null; audit_source_ips_last_success_at: string | null; extension_outbox_last_success_at: string | null };
};

export type DiagnosticThresholds = { pendingAgeSeconds: number; staleSeconds: number };
export const defaultDiagnosticThresholds: DiagnosticThresholds = { pendingAgeSeconds: 60, staleSeconds: 45 };

/** Grace alone is healthy. Counts are sampled across locks; warnings are an
 * investigation aid, while Prometheus examples require sustained conditions. */
export function diagnosticWarnings(snapshot: DiagnosticsSnapshot, thresholds = defaultDiagnosticThresholds, now = Date.now()): string[] {
  const warnings: string[] = [];
  const observed = snapshot.presence.observed_at && Date.parse(snapshot.presence.observed_at);
  if (!observed || !Number.isFinite(observed) || now - observed > thresholds.staleSeconds * 1000) warnings.push("Presence の観測が未完了、または古くなっています。");
  if (snapshot.presence.memberships_without_binding > 0) warnings.push("所有記録のない Membership があります。回収処理を確認してください。");
  if (snapshot.presence.bindings_without_membership > 0) warnings.push("Membership のない所有記録があります。");
  if (snapshot.presence.connection_ownership_mismatch > 0) warnings.push("参加済み接続と接続中の所有記録に差があります。再接続直後の一時差か確認してください。");
  if (snapshot.persistence.pending > 0 && snapshot.persistence.oldest_pending_seconds >= thresholds.pendingAgeSeconds) warnings.push("永続化待ちが設定した時間を超えています。");
  if (snapshot.extensions.dead_letters > 0) warnings.push("拡張配信の Dead Letter が残っています。");
  return warnings;
}

type Options = { load: () => Promise<DiagnosticsSnapshot> };
export type DiagnosticsController = { refresh: () => Promise<void>; clear: () => void };

/** Mount once. clear() invalidates in-flight requests when auth/server changes. */
export function mountDiagnostics(container: HTMLElement, options: Options): DiagnosticsController {
  const heading = document.createElement("h2"); heading.textContent = "運用診断";
  const intro = document.createElement("p"); intro.textContent = "再接続の猶予待ちは正常な状態です。不整合の件数と永続化の遅延を分けて確認できます。";
  const refresh = document.createElement("button"); refresh.type = "button"; refresh.className = "button secondary"; refresh.textContent = "診断を更新";
  const status = document.createElement("p"); status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  const settings = document.createElement("div"); settings.className = "diagnostic-settings";
  function threshold(label: string, value: number): HTMLInputElement {
    const wrap = document.createElement("label"); wrap.textContent = label;
    const input = document.createElement("input"); input.type = "number"; input.min = "1"; input.max = "86400"; input.value = String(value);
    wrap.append(input); settings.append(wrap); return input;
  }
  const pendingAge = threshold("永続化待ちの注意秒数", defaultDiagnosticThresholds.pendingAgeSeconds);
  const staleAge = threshold("観測の有効秒数", defaultDiagnosticThresholds.staleSeconds);
  const content = document.createElement("div"); content.className = "diagnostic-content";
  container.replaceChildren(heading, intro, refresh, settings, status, content);
  let generation = 0;
  let loading = false;
  let snapshot: DiagnosticsSnapshot | null = null;
  function readThreshold(input: HTMLInputElement, fallback: number): number {
    const value = Number(input.value);
    return Number.isFinite(value) && value >= 1 && value <= 86400 ? value : fallback;
  }
  function table(title: string, rows: [string, string | number][]): HTMLElement {
    const section = document.createElement("section");
    const h = document.createElement("h3"); h.textContent = title;
    const dl = document.createElement("dl");
    for (const [label, value] of rows) {
      const dt = document.createElement("dt"); dt.textContent = label;
      const dd = document.createElement("dd"); dd.textContent = String(value);
      dl.append(dt, dd);
    }
    section.append(h, dl); return section;
  }
  function render(): void {
    content.replaceChildren();
    if (!snapshot) return;
    const s = snapshot;
    const warnings = diagnosticWarnings(s, {
      pendingAgeSeconds: readThreshold(pendingAge, defaultDiagnosticThresholds.pendingAgeSeconds),
      staleSeconds: readThreshold(staleAge, defaultDiagnosticThresholds.staleSeconds),
    });
    const notices = document.createElement("ul"); notices.className = warnings.length ? "diagnostic-warnings" : "diagnostic-healthy";
    for (const warning of warnings.length ? warnings : ["この観測では不整合・遅延の注意条件に該当しません。継続的な監視は Prometheus を使用してください。"]) {
      const item = document.createElement("li"); item.textContent = warning; notices.append(item);
    }
    const cards = document.createElement("div"); cards.className = "diagnostic-grid";
    cards.append(
      table("Presence", [["接続中", s.presence.connected], ["猶予待ち（正常）", s.presence.grace_waiting], ["回収の応答待ち", s.presence.reclaim_waiting], ["Membership", s.presence.memberships], ["参加済み接続", s.presence.joined_connections]]),
      table("所有と参加の不整合", [["所有記録なし", s.presence.memberships_without_binding], ["Membership なし", s.presence.bindings_without_membership], ["接続と所有の差", s.presence.connection_ownership_mismatch], ["観測時刻", s.presence.observed_at ?? "未観測"]]),
      table("永続化待ち", [["待ち作業の合計", s.persistence.pending], ["最も古い作業の秒数", s.persistence.oldest_pending_seconds], ["Entity 効果", s.persistence.entity_events], ["コマンド結果", s.persistence.command_outcomes], ["Generation Snapshot", s.persistence.generation_snapshots]]),
      table("Resume 拒否（起動後の累計）", [["トークンの不一致・欠落", s.resume_rejections_total.token_mismatch], ["履歴の利用不可", s.resume_rejections_total.history_unavailable], ["Revision の欠落", s.resume_rejections_total.revision_gap], ["未来の Revision", s.resume_rejections_total.future_revision], ["認可拒否", s.resume_rejections_total.not_authorized], ["所有世代の喪失", s.resume_rejections_total.ownership_lost]]),
      table("Checkpoint と拡張", [["最新 Checkpoint", s.checkpoint.latest_at ?? "未保存"], ["保存失敗累計", s.checkpoint.save_failures_total], ["復元失敗累計", s.checkpoint.restore_failures_total], ["拡張 Outbox", s.extensions.outbox_pending], ["配信中", s.extensions.deliveries_in_flight], ["Dead Letter", s.extensions.dead_letters]]),
      table("キュー（最後の観測 / 容量）", [["Control", `${s.queues.control.last_observed_depth} / ${s.queues.control.capacity_per_instance}`], ["Transform", `${s.queues.transform.last_observed_depth} / ${s.queues.transform.capacity_per_instance}`], ["Entity", `${s.queues.entity.last_observed_depth} / ${s.queues.entity.capacity_per_instance}`], ["Outbound", `${s.queues.outbound.last_observed_depth} / ${s.queues.outbound.capacity_per_connection}`]])
    );
    const note = document.createElement("p"); note.textContent = "件数はサーバー単位の観測値です。キュー値は最後に観測した対象の深さで、全 Instance の合計ではありません。永続化の各段階は別作業として数え、Entity バッチの待ち時間はキューから保存処理への移動と再試行でも引き継ぎます。";
    content.append(notices, cards, note);
  }
  const controller: DiagnosticsController = {
    async refresh() {
      if (loading) return;
      const current = generation;
      loading = true; refresh.disabled = true; status.textContent = "診断を取得しています…";
      try {
        const result = await options.load();
        if (current !== generation) return;
        snapshot = result; render(); status.textContent = `取得時刻: ${result.collected_at}（手動更新）`;
      } catch (error) {
        if (current !== generation) return;
        snapshot = null; content.replaceChildren();
        const code = error && typeof error === "object" && "status" in error ? Number(error.status) : 0;
        status.textContent = code === 403 ? "診断を読む権限がありません（admin.diagnostics.read）。" : "診断を取得できませんでした。権限とサーバーの状態を確認して再試行してください。";
      } finally {
        if (current === generation) { loading = false; refresh.disabled = false; }
      }
    },
    clear() {
      generation += 1; loading = false; snapshot = null; refresh.disabled = false;
      content.replaceChildren(); status.textContent = "診断を表示するには更新してください。";
    },
  };
  refresh.addEventListener("click", () => { void controller.refresh(); });
  pendingAge.addEventListener("change", render); staleAge.addEventListener("change", render);
  controller.clear();
  return controller;
}
