import assert from "node:assert/strict";
import test from "node:test";
import {
  normalizeGithubMirrorUrl,
  readClientGithubMirror,
  saveClientGithubMirror,
} from "../src/lib/github-mirror.ts";

test("normalizes GitHub mirror base URLs", () => {
  assert.equal(normalizeGithubMirrorUrl(""), "");
  assert.equal(normalizeGithubMirrorUrl("  https://gh-proxy.com  "), "https://gh-proxy.com/");
  assert.equal(normalizeGithubMirrorUrl("https://gh-proxy.com/"), "https://gh-proxy.com/");
  assert.equal(
    normalizeGithubMirrorUrl("http://127.0.0.1:8080/proxy"),
    "http://127.0.0.1:8080/proxy/",
  );
});

test("rejects unsupported mirror protocols", () => {
  assert.throws(() => normalizeGithubMirrorUrl("ftp://example.com/mirror"));
});

test("persists the client mirror independently from server settings", () => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
  assert.equal(saveClientGithubMirror("https://gh-proxy.org"), "https://gh-proxy.org/");
  assert.equal(readClientGithubMirror(), "https://gh-proxy.org/");
  assert.equal(saveClientGithubMirror(""), "");
  assert.equal(readClientGithubMirror(), "");
});
