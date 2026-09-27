/**
 * Realtime ticket contract — single source of truth for `TicketResponse`.
 *
 * OpenAPI source: `openapi/orbisync-v1.yaml` components.schemas.TicketResponse
 *   required: [realtime_ticket, expires_in]
 *   properties:
 *     realtime_ticket: string
 *     expires_in: integer minimum 1
 *
 * Rust source: `crates/orbisync-transport-http/src/auth.rs` struct TicketResponse
 *   { realtime_ticket: String, expires_in: u64 }
 *
 * This file centralizes the shape so that `client.ts`, tests, and examples
 * do not hand-write `{ ticket, expires_at }` divergently. Any drift is
 * caught by `scripts/check_realtime_ticket_contract.py` and by the unit
 * test in `realtime_ticket.test.ts` which validates a fixture derived from
 * the OpenAPI schema.
 */

export type RealtimeTicketResponse = {
  realtime_ticket: string;
  expires_in: number;
};

/**
 * Error thrown when `POST /v1/realtime/tickets` is rate limited (429).
 * Maps to `RATE_LIMITED` / HTTP 429 per `openapi/errors.yaml` and
 * `crates/orbisync-transport-http/src/auth.rs` (`ErrorCode::RateLimited`).
 * Callers can `instanceof RealtimeTicketRateLimitedError` to distinguish
 * rate limiting from other failures (401, 500) and apply backoff.
 */
export class RealtimeTicketRateLimitedError extends Error {
  readonly status = 429 as const;
  readonly code = "RATE_LIMITED" as const;
  constructor(message = "realtime ticket rate limited") {
    super(message);
    this.name = "RealtimeTicketRateLimitedError";
  }
}

/**
 * Parse and validate an unknown JSON value as `RealtimeTicketResponse`.
 * Throws if required fields are missing or have wrong types, matching the
 * 400/500 boundary that the real server enforces via `additionalProperties: false`.
 */
export function parseRealtimeTicketResponse(data: unknown): RealtimeTicketResponse {
  if (typeof data !== "object" || data === null) {
    throw new Error("ticket response is not an object");
  }
  const obj = data as Record<string, unknown>;
  const rt = obj["realtime_ticket"];
  const ei = obj["expires_in"];
  if (typeof rt !== "string" || rt.length === 0) {
    throw new Error("ticket response missing realtime_ticket field");
  }
  if (typeof ei !== "number" || !Number.isInteger(ei) || ei < 1) {
    throw new Error("ticket response missing expires_in field");
  }
  return { realtime_ticket: rt, expires_in: ei };
}

/** Extract just the ticket string, validating the envelope. */
export function extractRealtimeTicket(data: unknown): string {
  return parseRealtimeTicketResponse(data).realtime_ticket;
}
