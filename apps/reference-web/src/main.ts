import "./style.css";
import { Adventure, BEACON, STAR_POSITIONS, timeLabel } from "./game";
import { makeScene } from "./scene";
import type { OnlineSession, Remote } from "./online";

const el = <T extends HTMLElement = HTMLElement>(id: string) =>
    document.getElementById(id) as T;
const game = new Adventure();
const keys = new Set<string>();
const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;
let view: ReturnType<typeof makeScene>;
try {
    view = makeScene(el("world"));
} catch {
    const error = document.createElement("div");
    error.className = "fatal";
    error.setAttribute("role", "alert");
    error.textContent =
        "3D画面を開始できませんでした。ブラウザのハードウェアアクセラレーションを有効にして、再読み込みしてください。";
    document.body.append(error);
    throw new Error("WebGL initialization failed");
}
view.renderer.domElement.addEventListener("webglcontextlost", (event) => {
    event.preventDefault();
    keys.clear();
    toast("3D描画が停止しました。ページを再読み込みしてください。", 60000);
});
let session: OnlineSession | undefined;
const others = new Map<
    string,
    { avatar: ReturnType<typeof view.avatar>; target: Remote; seen: number }
>();
let angle = 0;
let sound = false;
let audio: AudioContext | undefined;
let toastTimer = 0;
function toast(message: string, duration = 3300) {
    el("toast").textContent = message;
    el("toast").classList.add("visible");
    clearTimeout(toastTimer);
    toastTimer = window.setTimeout(
        () => el("toast").classList.remove("visible"),
        duration,
    );
}
function chime(notes: number[]) {
    if (!sound) return;
    audio ??= new AudioContext();
    void audio.resume();
    notes.forEach((hz, i) => {
        const oscillator = audio!.createOscillator(),
            gain = audio!.createGain(),
            t = audio!.currentTime + i * 0.12;
        oscillator.type = "sine";
        oscillator.frequency.value = hz;
        gain.gain.setValueAtTime(0, t);
        gain.gain.linearRampToValueAtTime(0.065, t + 0.015);
        gain.gain.exponentialRampToValueAtTime(0.001, t + 0.55);
        oscillator.connect(gain);
        gain.connect(audio!.destination);
        oscillator.start(t);
        oscillator.stop(t + 0.6);
    });
}
function dialogOpen() {
    return !!document.querySelector("dialog[open]");
}
function playable() {
    return (
        game.started && !dialogOpen() && el("finish").hidden && !document.hidden
    );
}
function progress() {
    el("count").innerHTML = `${game.collected.size} <span>/ 5</span>`;
    el("progress").style.width = `${(game.collected.size / 5) * 100}%`;
    document
        .querySelector(".progress")!
        .setAttribute("aria-valuenow", String(game.collected.size));
    el("objective").textContent =
        game.collected.size === 5
            ? "灯台にあかりをともす"
            : "星のかけらを集める";
    el("hint").textContent =
        game.collected.size === 5
            ? "中央の灯台へ戻って、Eキーを押そう。"
            : "光るかけらに近づくと、拾えます。";
}
function start() {
    // Keep online avatars in place: resetting progress must not send a teleport.
    const position =
        session && game.started
            ? { x: game.x, z: game.z, y: game.y, velocityY: game.velocityY }
            : null;
    game.reset();
    if (position) Object.assign(game, position);
    keys.clear();
    angle = 0;
    el("welcome").hidden = true;
    el("finish").hidden = true;
    el("hud").hidden = false;
    el("controls").hidden = false;
    el("touch").hidden = false;
    view.setPlaying();
    progress();
    toast("ようこそ！ 金色のかけらを探して歩いてみよう。");
    (document.activeElement as HTMLElement)?.blur();
}
el("start").onclick = start;
el("again").onclick = start;
el("restart").onclick = start;
el("explore").onclick = () => {
    el("finish").hidden = true;
    el("objective").textContent = "灯台にあかりが戻りました";
    el("hint").textContent = "島の景色を、ゆっくり楽しんでください。";
};
function ignite() {
    if (!playable() || !game.ignite()) return;
    keys.clear();
    el("interaction").hidden = true;
    el("finish").hidden = false;
    el("result").textContent =
        `かけら 5 / 5　 ·　冒険の時間 ${timeLabel(game.elapsed)}`;
    chime([523.25, 659.25, 783.99, 1046.5]);
    el("again").focus();
}
el("ignite").onclick = ignite;
el("sound").onclick = () => {
    sound = !sound;
    el("sound").textContent = `音：${sound ? "オン" : "オフ"}`;
    el("sound").setAttribute("aria-pressed", String(sound));
    if (sound) chime([659, 880]);
};
el("help").onclick = () => {
    keys.clear();
    el<HTMLDialogElement>("help-dialog").showModal();
};
el("online").onclick = () => {
    keys.clear();
    el<HTMLDialogElement>("online-dialog").showModal();
};
document
    .querySelectorAll<HTMLButtonElement>("[data-close]")
    .forEach(
        (button) => (button.onclick = () => button.closest("dialog")?.close()),
    );
const movementKeys = new Set([
    "w",
    "a",
    "s",
    "d",
    "arrowup",
    "arrowdown",
    "arrowleft",
    "arrowright",
    "shift",
    " ",
    "e",
]);
window.addEventListener("keydown", (event) => {
    if (
        !playable() ||
        event.target instanceof HTMLInputElement ||
        !movementKeys.has(event.key.toLowerCase())
    )
        return;
    event.preventDefault();
    const key = event.key.toLowerCase();
    keys.add(key);
    if (!event.repeat) {
        if (key === " ") game.jump();
        if (key === "e") ignite();
    }
});
window.addEventListener("keyup", (event) =>
    keys.delete(event.key.toLowerCase()),
);
window.addEventListener("blur", () => keys.clear());
document.addEventListener("visibilitychange", () => keys.clear());
document.querySelectorAll<HTMLButtonElement>("[data-key]").forEach((button) => {
    button.onpointerdown = (event) => {
        event.preventDefault();
        if (!playable()) return;
        button.setPointerCapture(event.pointerId);
        keys.add(button.dataset.key!);
        if (button.dataset.key === " ") game.jump();
    };
    const release = () => keys.delete(button.dataset.key!);
    button.onpointerup = release;
    button.onpointercancel = release;
    button.onlostpointercapture = release;
});
function remote(entity: Remote) {
    let other = others.get(entity.id);
    if (!other) {
        other = {
            avatar: view.avatar("#ab98cf"),
            target: entity,
            seen: performance.now(),
        };
        other.avatar.g.position.set(entity.x, entity.y, entity.z);
        others.set(entity.id, other);
    }
    other.target = entity;
    other.seen = performance.now();
}
function removeOthers() {
    for (const other of others.values()) {
        view.scene.remove(other.avatar.g);
        other.avatar.g.traverse((object) => {
            if ("geometry" in object)
                (object.geometry as { dispose(): void }).dispose();
        });
    }
    others.clear();
}
el<HTMLFormElement>("connect-form").onsubmit = async (event) => {
    event.preventDefault();
    const button = el<HTMLButtonElement>("connect");
    button.disabled = true;
    button.textContent = "島に接続しています…";
    el("connection-error").textContent = "";
    try {
        const { OnlineSession } = await import("./online");
        const candidate = new OnlineSession(remote, (message) => {
            session = undefined;
            removeOthers();
            el("mode").textContent = "ひとりで探索（通信終了）";
            toast(message, 7000);
        });
        await candidate.connect(
            el<HTMLInputElement>("server").value.trim(),
            el<HTMLInputElement>("login").value.trim(),
            el<HTMLInputElement>("password").value,
            el<HTMLInputElement>("room").value.trim(),
        );
        if (!el<HTMLDialogElement>("online-dialog").open) {
            await candidate.close();
            return;
        }
        session = candidate;
        el<HTMLInputElement>("password").value = "";
        el<HTMLDialogElement>("online-dialog").close();
        start();
        el("mode").textContent = "オンライン · 接続済み";
        toast("同じルームの仲間と探索できます。");
    } catch (error) {
        const detail = error instanceof Error ? error.message : "";
        el("connection-error").textContent = /401/.test(detail)
            ? "ログインIDまたはパスワードを確認してください。"
            : /403/.test(detail)
              ? "このルームに参加する権限がありません。"
              : /UUID|サーバーURL/.test(detail)
                ? detail
                : "接続できませんでした。サーバーの起動・URL・ルームIDとブラウザ接続の許可設定を確認してください。";
    } finally {
        button.disabled = false;
        button.textContent = "接続して冒険する →";
    }
};
window.addEventListener("pagehide", () => {
    void session?.close();
});
let previous = performance.now(),
    uiTime = 0;
function frame(now: number) {
    const dt = Math.min((now - previous) / 1000, 0.25);
    previous = now;
    let moving = false;
    if (playable()) {
        const dx =
            Number(keys.has("d") || keys.has("arrowright")) -
            Number(keys.has("a") || keys.has("arrowleft"));
        const dz =
            Number(keys.has("s") || keys.has("arrowdown")) -
            Number(keys.has("w") || keys.has("arrowup"));
        moving = !!(dx || dz);
        if (moving) angle = Math.atan2(dx, dz);
        const picked: number[] = [];
        for (let remaining = dt; remaining > 0; remaining -= 0.025)
            picked.push(
                ...game.step(
                    Math.min(remaining, 0.025),
                    dx,
                    dz,
                    keys.has("shift"),
                ),
            );
        if (picked.length) {
            progress();
            chime([660, 880, 1100]);
            toast(
                game.collected.size === 5
                    ? "５つそろった！ 中央の灯台に戻ろう。"
                    : `星のかけらを見つけた！　${game.collected.size} / 5`,
            );
        }
    }
    view.player.g.rotation.y = angle;
    let target: { x: number; z: number } | null =
        game.collected.size === 5 ? BEACON : null;
    if (!target) {
        let distance = Infinity;
        STAR_POSITIONS.forEach(([x, z], i) => {
            const d = Math.hypot(x - game.x, z - game.z);
            if (!game.collected.has(i) && d < distance) {
                distance = d;
                target = { x, z };
            }
        });
    }
    el("interaction").hidden = !playable() || !game.canIgnite || game.complete;
    if (now - uiTime > 150) {
        uiTime = now;
        el("timer").textContent = timeLabel(game.elapsed);
        if (target)
            el("target").textContent = game.complete
                ? "✦ 灯台が点灯しました"
                : `${game.collected.size === 5 ? "♧ 灯台" : "✧ 次のかけら"}まで ${Math.ceil(Math.hypot(target.x - game.x, target.z - game.z))} m`;
    }
    session?.tick(now, { x: game.x, y: game.y, z: game.z }, angle);
    for (const [id, other] of others) {
        if (now - other.seen > 12000) {
            view.scene.remove(other.avatar.g);
            other.avatar.g.traverse((object) => {
                if ("geometry" in object)
                    (object.geometry as { dispose(): void }).dispose();
            });
            others.delete(id);
            continue;
        }
        const t = other.target;
        other.avatar.g.position.x +=
            (t.x - other.avatar.g.position.x) * (1 - Math.exp(-dt * 12));
        other.avatar.g.position.z +=
            (t.z - other.avatar.g.position.z) * (1 - Math.exp(-dt * 12));
        other.avatar.g.position.y = t.y + 0.1;
        other.avatar.g.rotation.y = t.angle;
    }
    view.render(now / 1000, dt, game, moving, target, reduced);
    requestAnimationFrame(frame);
}
requestAnimationFrame(frame);
