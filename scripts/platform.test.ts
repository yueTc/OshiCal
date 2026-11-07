import assert from "node:assert/strict";
import test from "node:test";
import { detectMobileShell, resolveDesktopShell } from "../src/lib/platform.ts";

test("recognises Android and iOS user agents", () => {
  assert.equal(
    detectMobileShell(
      "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/126.0 Mobile Safari/537.36",
    ),
    true,
  );
  assert.equal(
    detectMobileShell(
      "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148",
    ),
    true,
  );
});

test("keeps desktop user agents on the desktop shell", () => {
  assert.equal(
    detectMobileShell(
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 OshiCal/0.3.6",
    ),
    false,
  );
  assert.equal(detectMobileShell(""), false);
});

test("desktop shell requires Tauri outside a mobile webview", () => {
  assert.equal(resolveDesktopShell(true, false), true);
  assert.equal(resolveDesktopShell(true, true), false);
  assert.equal(resolveDesktopShell(false, false), false);
  assert.equal(resolveDesktopShell(false, true), false);
});
