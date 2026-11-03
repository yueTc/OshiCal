import assert from "node:assert/strict";
import test from "node:test";
import { httpBaseFromWebSocketUrl } from "../src/lib/server-url.ts";

test("maps secure WebSocket origin to HTTPS", () => {
  assert.equal(
    httpBaseFromWebSocketUrl("wss://example.com/api/ws?after=0&protocol=2"),
    "https://example.com",
  );
});

test("maps WebSocket origin to HTTP and preserves a base path", () => {
  assert.equal(
    httpBaseFromWebSocketUrl("ws://127.0.0.1:8787/oshical/api/ws"),
    "http://127.0.0.1:8787/oshical",
  );
});

test("does not turn an HTTP base into a WebSocket URL", () => {
  assert.equal(
    httpBaseFromWebSocketUrl("https://example.com:9443/api/ws"),
    "https://example.com:9443",
  );
});
