import { test, expect } from "@playwright/test";
import {
    create,
    fromBinary,
    toBinary,
    type MessageInitShape,
} from "@bufbuild/protobuf";
import { EnvelopeSchema } from "../../../sdk/typescript/src/generated/orbisync/v1/realtime_pb";

test("実SDKで認証・protobuf入室・リビジョン付き位置送信を行う", async ({
    page,
}) => {
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.route("http://localhost:8080/v1/**", async (route) => {
        const ticket = route.request().url().endsWith("/realtime/tickets");
        if (ticket)
            expect(route.request().headers().authorization).toBe(
                "Bearer test-access",
            );
        await route.fulfill({
            json: ticket
                ? { realtime_ticket: "test-ticket", expires_in: 60 }
                : {
                      access_token: "test-access",
                      refresh_token: "test-refresh",
                      expires_in: 900,
                  },
            headers: { "Access-Control-Allow-Origin": "*" },
        });
    });
    let revision = 0n,
        updates = 0;
    const violations: string[] = [];
    await page.routeWebSocket("ws://localhost:8080/ws", (socket) => {
        let sequence = 0n;
        const send = (
            payload: MessageInitShape<typeof EnvelopeSchema>["payload"],
        ) =>
            socket.send(
                Buffer.from(
                    toBinary(
                        EnvelopeSchema,
                        create(EnvelopeSchema, {
                            protocolMajor: 1,
                            messageId: crypto.randomUUID(),
                            sequence: ++sequence,
                            payload,
                        }),
                    ),
                ),
            );
        socket.onMessage((message) => {
            const env = fromBinary(
                EnvelopeSchema,
                typeof message === "string"
                    ? new TextEncoder().encode(message)
                    : message,
            );
            const p = env.payload;
            if (p.case === "clientHello") {
                if (p.value.realtimeTicket !== "test-ticket")
                    violations.push("ticket");
                send({
                    case: "serverHello",
                    value: {
                        connectionId: "test",
                        heartbeatIntervalMs: 20000n,
                    },
                });
            }
            if (p.case === "joinInstance")
                send({
                    case: "joinAccepted",
                    value: {
                        presenceId: "test-presence",
                        instanceRevision: 0n,
                        entitySpawn: true,
                        entityUpdateOwn: true,
                    },
                });
            if (p.case === "transformInput") {
                if (!/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(p.value.entityId))
                    violations.push("entity ID must be UUIDv7");
                if (p.value.expectedRevision !== revision)
                    violations.push(
                        `revision ${p.value.expectedRevision} / ${revision}`,
                    );
                updates++;
                revision++;
                const response = create(EnvelopeSchema, {
                    payload: {
                        case: "stateDelta",
                        value: {
                            fromRevision: revision - 1n,
                            toRevision: revision,
                            entities: [
                                {
                                    entityId: p.value.entityId,
                                    revision,
                                    transform: p.value.transform,
                                },
                            ],
                        },
                    },
                });
                send(response.payload);
            }
        });
    });
    await page.goto("/");
    await page.getByRole("button", { name: "友だちと同じ島へ" }).click();
    await page.locator("#login").fill("demo");
    await page.locator("#password").fill("test-password");
    await page.locator("#room").fill("00000000-0000-7000-8000-000000000001");
    await page.locator("#connect").click();
    await expect(page.locator("#mode")).toContainText("オンライン", { timeout: 20000 });
    await expect.poll(() => updates, { timeout: 10000 }).toBeGreaterThanOrEqual(3);
    expect(violations).toEqual([]);
    expect(errors).toEqual([]);
});
