import { v7 as uuidv7 } from "uuid";
import {
    OrbiSyncClient,
    SyncError,
    type OrbiSyncConnection,
    type OrbiSyncInstance,
} from "../../../sdk/typescript/src/client";
import type {
    EntityState,
    Snapshot,
} from "../../../sdk/typescript/src/generated/orbisync/v1/realtime_pb";

export type Remote = {
    id: string;
    x: number;
    y: number;
    z: number;
    angle: number;
};
export class OnlineSession {
    private connection?: OrbiSyncConnection;
    private instance?: OrbiSyncInstance;
    private entityId = uuidv7();
    private revision = 0n;
    private pendingSince = 0;
    private lastSent = 0;
    private closed = false;
    private ready = false;
    private chunks = new Map<number, Uint8Array>();
    private snapshotId = "";
    constructor(
        private remote: (entity: Remote) => void,
        private failure: (message: string) => void,
    ) {}
    async connect(
        baseUrl: string,
        loginId: string,
        password: string,
        room: string,
    ) {
        const url = new URL(baseUrl);
        if (
            !["http:", "https:"].includes(url.protocol) ||
            url.username ||
            url.password
        )
            throw new Error("サーバーURLを確認してください。");
        if (
            !/^[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}$/i.test(
                room,
            )
        )
            throw new Error("ルームIDはUUID形式で入力してください。");
        const client = new OrbiSyncClient({
            baseUrl: url.href.replace(/\/$/, ""),
            clientName: "hoshiakari-island",
        });
        try {
            await client.auth.login({ loginId, password });
            this.connection = await client.connect();
            this.instance = await this.connection.join(room);
            this.instance.on("entityUpdated", (raw) =>
                this.update(raw as EntityState),
            );
            this.instance.on("snapshot", (raw) =>
                this.snapshot(raw as Snapshot),
            );
            // Runtime SDK dispatches protocol errors too; its public overloads only list data events.
            const events = this.instance as unknown as {
                on(event: string, handler: (value: unknown) => void): void;
            };
            events.on("error", () =>
                this.fail(
                    "サーバーが移動を受け付けませんでした。ルームの権限や移動制限を確認してください。",
                ),
            );
            events.on("resyncRequired", () =>
                this.fail(
                    "再入室が必要です。ページを再読み込みして接続してください。",
                ),
            );
            // Subscribe before waiting so the initial snapshot is observed.
            try {
                await this.instance.ready();
            } catch (error) {
                // Legacy servers do not support the SDK readiness barrier.
                if (!(error instanceof SyncError) || error.code !== "UNSUPPORTED_SERVER")
                    throw error;
            }
            this.ready = !this.closed;
        } catch (error) {
            await this.close();
            throw error;
        }
    }
    private update(e: EntityState) {
        if (e.entityId === this.entityId) {
            this.revision = e.revision;
            this.pendingSince = 0;
            return;
        }
        const t = e.transform;
        if (t && [t.positionX, t.positionY, t.positionZ].every(Number.isFinite))
            this.remote({
                id: e.entityId,
                x: t.positionX,
                y: t.positionY,
                z: t.positionZ,
                angle: 2 * Math.atan2(t.rotationY, t.rotationW),
            });
    }
    private snapshot(s: Snapshot) {
        if (s.snapshotId !== this.snapshotId) {
            this.chunks.clear();
            this.snapshotId = s.snapshotId;
        }
        if (s.chunkCount > 1024 || s.data.byteLength > 4_194_304) return;
        this.chunks.set(s.chunkIndex, s.data);
        if (this.chunks.size !== s.chunkCount) return;
        try {
            const parts = Array.from(
                { length: s.chunkCount },
                (_, i) => this.chunks.get(i)!,
            );
            const bytes = new Uint8Array(
                parts.reduce((n, p) => n + p.length, 0),
            );
            let offset = 0;
            for (const p of parts) {
                bytes.set(p, offset);
                offset += p.length;
            }
            const value = JSON.parse(new TextDecoder().decode(bytes)) as {
                entities?: {
                    entity_id: string;
                    transform?: {
                        position: { x: number; y: number; z: number };
                        rotation: { y: number; w: number };
                    };
                }[];
            };
            for (const e of value.entities ?? []) {
                const t = e.transform;
                if (
                    t &&
                    e.entity_id !== this.entityId &&
                    [t.position.x, t.position.y, t.position.z].every(
                        Number.isFinite,
                    )
                )
                    this.remote({
                        id: e.entity_id,
                        ...t.position,
                        angle: 2 * Math.atan2(t.rotation.y, t.rotation.w),
                    });
            }
        } catch {
            this.fail("島の状態を読み込めませんでした。再接続してください。");
        }
        this.chunks.clear();
    }
    tick(
        now: number,
        position: { x: number; y: number; z: number },
        angle: number,
    ) {
        if (!this.instance || this.closed || !this.ready) return;
        if (this.pendingSince) {
            // Unchanged transforms advance revision without entityUpdated.
            const applied = this.instance.entityRevision(this.entityId);
            if (applied !== undefined && applied > this.revision) {
                this.revision = applied;
                this.pendingSince = 0;
            }
        }
        if (this.pendingSince) {
            if (now - this.pendingSince > 8000)
                this.fail("通信が途切れました。ひとりでの探索を続けられます。");
            return;
        }
        if (now - this.lastSent < 100) return;
        try {
            this.instance.sendTransform({
                entityId: this.entityId,
                expectedRevision: this.revision,
                position,
                rotation: {
                    x: 0,
                    y: Math.sin(angle / 2),
                    z: 0,
                    w: Math.cos(angle / 2),
                },
            });
            this.lastSent = now;
            this.pendingSince = now;
        } catch {
            this.fail("通信が途切れました。ひとりでの探索を続けられます。");
        }
    }
    private fail(message: string) {
        if (this.closed) return;
        void this.close();
        this.failure(message);
    }
    async close() {
        this.closed = true;
        await this.connection?.disconnect();
        this.instance = undefined;
        this.chunks.clear();
    }
}
