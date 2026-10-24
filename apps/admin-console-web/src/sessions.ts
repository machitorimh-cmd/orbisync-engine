import { OrbiSyncAdminApi, type ManagedSession } from "./api.js";

export function mountSessionPanel(api: OrbiSyncAdminApi, root: HTMLElement,
  onError: (error: unknown) => void, onCurrentRevoked: () => void): { reset(): void } {
  const el = <T extends HTMLElement>(id: string) => root.querySelector<T>("#" + id)!;
  const body = el<HTMLTableSectionElement>("sessionsTbody"), status = el<HTMLElement>("sessionsStatus");
  const target = el<HTMLInputElement>("sessionTargetUser"), next = el<HTMLButtonElement>("nextSessionsBtn");
  let generation = 0, user: string | undefined, cursor: string | null = null, loading = false;
  const date = (value: string) => new Date(value).toLocaleString();
  const reset = () => { generation++; cursor = null; loading = false; user = undefined; body.replaceChildren(); next.hidden = true; status.textContent = "一覧を取得してください。"; target.value = ""; };
  const busy = async (button: HTMLButtonElement, action: () => Promise<void>) => {
    button.disabled = true;
    try { await action(); } catch (error) { onError(error); }
    finally { button.disabled = false; }
  };
  const load = async (fresh: boolean) => {
    if (loading && !fresh) return;
    if (fresh) { generation++; cursor = null; body.replaceChildren(); next.hidden = true; }
    const epoch = generation, owner = user; loading = true;
    try {
      const page = await api.listSessions(cursor, owner);
      if (epoch !== generation) return;
      for (const session of page.items) append(session, owner, epoch);
      cursor = page.next_cursor ?? null; next.hidden = !cursor;
      status.textContent = owner ? "管理対象: " + owner : "自分のセッション（現在の接続を明示）";
    } finally { if (epoch === generation) loading = false; }
  };
  const append = (session: ManagedSession, owner: string | undefined, epoch: number) => {
    const row = document.createElement("tr");
    const cell = (text: string) => { const td = document.createElement("td"); td.textContent = text; return td; };
    const id = cell(session.session_id + (session.current ? "（現在）" : ""));
    const label = cell(session.device_label ?? "未設定"), actions = document.createElement("td");
    if (!owner) {
      const input = document.createElement("input"); input.value = session.device_label ?? ""; input.maxLength = 128; input.setAttribute("aria-label", "端末の表示名");
      const save = document.createElement("button"); save.textContent = "名前を保存"; save.className = "button secondary";
      save.onclick = () => { void busy(save, async () => {
        await api.labelSession(session.session_id, input.value.trim() || null);
        if (epoch === generation) await load(true);
      }); };
      label.replaceChildren(input, save);
    }
    const revoke = document.createElement("button"); revoke.textContent = "失効"; revoke.className = "button danger-ghost";
    revoke.disabled = session.status === "revoked";
    revoke.onclick = () => { void busy(revoke, async () => {
      await api.revokeSession(session.session_id, owner);
      if (session.current) { api.clearSession(); onCurrentRevoked(); return; }
      if (epoch === generation) await load(true);
    }); };
    actions.append(revoke); row.append(id,label,cell(session.status),cell(date(session.created_at)),cell(date(session.expires_at)),actions); body.append(row);
  };
  const bind = (id: string, action: () => Promise<void>) => {
    const button = el<HTMLButtonElement>(id); button.onclick = () => { void busy(button, action); };
  };
  bind("selfSessionsBtn", async () => { user = undefined; await load(true); });
  bind("adminSessionsBtn", async () => {
    const value = target.value.trim(); if (!value) throw new Error("管理対象ユーザーIDを入力してください");
    user = value; await load(true);
  });
  bind("nextSessionsBtn", () => load(false));
  bind("revokeOtherSessionsBtn", async () => { await api.revokeOtherSessions(); user = undefined; await load(true); });
  bind("revokeAllUserSessionsBtn", async () => {
    const value = target.value.trim(); if (!value) throw new Error("管理対象ユーザーIDを入力してください");
    await api.revokeUserSessions(value); user = value; await load(true);
  });
  target.oninput = () => { generation++; cursor = null; loading = false; user = undefined; body.replaceChildren(); next.hidden = true; };
  return { reset };
}
