import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseRealtimeTicketResponse, extractRealtimeTicket } from "./realtime_ticket.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

describe("RealtimeTicketResponse contract", () => {
  it("parses production response body (realtime_ticket / expires_in)", () => {
    const body = { realtime_ticket: "tok-abc", expires_in: 60 };
    const parsed = parseRealtimeTicketResponse(body);
    assert.equal(parsed.realtime_ticket, "tok-abc");
    assert.equal(parsed.expires_in, 60);
  });

  it("rejects legacy shape ticket/expires_at", () => {
    const legacy = { ticket: "tok-abc", expires_at: "2099-01-01T00:00:00Z" };
    assert.throws(() => parseRealtimeTicketResponse(legacy), /realtime_ticket/);
  });

  it("rejects missing realtime_ticket", () => {
    assert.throws(() => parseRealtimeTicketResponse({ expires_in: 60 }), /realtime_ticket/);
    assert.throws(() => parseRealtimeTicketResponse({ ticket: "x", expires_in: 60 }), /realtime_ticket/);
  });

  it("rejects wrong expires_in", () => {
    assert.throws(() => parseRealtimeTicketResponse({ realtime_ticket: "x", expires_at: "2099-01-01T00:00:00Z" }), /expires_in/);
    assert.throws(() => parseRealtimeTicketResponse({ realtime_ticket: "x" }), /expires_in/);
  });

  it("extracts ticket string via helper", () => {
    assert.equal(extractRealtimeTicket({ realtime_ticket: "t", expires_in: 60 }), "t");
  });

  it("fixture derived from OpenAPI TicketResponse validates", () => {
    // Load OpenAPI and verify required fields, then ensure fixture matches.
    const openapiPath = path.resolve(__dirname, "../../../openapi/orbisync-v1.yaml");
    const yaml = readFileSync(openapiPath, "utf-8");
    // Minimal parse without yaml library: check required strings exist
    assert.match(yaml, /TicketResponse:/);
    assert.match(yaml, /realtime_ticket:/);
    assert.match(yaml, /expires_in:/);
    // Ensure openapi requires both
    const ticketSection = yaml.slice(yaml.indexOf("TicketResponse:"));
    assert.match(ticketSection, /required:\s*\n\s*- realtime_ticket\s*\n\s*- expires_in/);

    // Production fixture must parse
    const fixture: unknown = { realtime_ticket: "prod-ticket", expires_in: 60 };
    const parsed = parseRealtimeTicketResponse(fixture);
    assert.equal(parsed.realtime_ticket, "prod-ticket");
  });

  it("production handler shape via auth.rs matches openapi", () => {
    // Read Rust handler to ensure it uses correct field names (contract, not variable name)
    const rustPath = path.resolve(__dirname, "../../../crates/orbisync-transport-http/src/auth.rs");
    const rust = readFileSync(rustPath, "utf-8");
    assert.match(rust, /realtime_ticket:\s*\w+\.expose_secret/);
    assert.match(rust, /expires_in:\s*60/);
    // Must not use legacy names in TicketResponse struct (allow other structs but not this)
    const ticketStruct = rust.slice(rust.indexOf("struct TicketResponse"));
    const structEnd = ticketStruct.indexOf("}");
    const structBody = ticketStruct.slice(0, structEnd);
    assert.match(structBody, /realtime_ticket/);
    assert.match(structBody, /expires_in/);
  });
});
