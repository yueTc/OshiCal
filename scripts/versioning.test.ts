import assert from "node:assert/strict";
import test from "node:test";
import { nextBetaVersion, promoteStableVersion } from "./versioning.mjs";

test("next beta advances exactly one patch from stable", () => {
  assert.equal(nextBetaVersion("0.3.20"), "0.3.21-beta.1");
});

test("next beta keeps the patch while incrementing beta number", () => {
  assert.equal(nextBetaVersion("0.3.21-beta.1"), "0.3.21-beta.2");
  assert.equal(nextBetaVersion("0.3.21-beta.9"), "0.3.21-beta.10");
});

test("promote stable removes only the beta suffix", () => {
  assert.equal(promoteStableVersion("0.3.21-beta.2"), "0.3.21");
  assert.throws(() => promoteStableVersion("0.3.21"), /只支持 X\.Y\.Z-beta\.N/);
});

test("unexpected prerelease labels are rejected", () => {
  assert.throws(() => nextBetaVersion("0.3.21-rc.1"), /next-beta 只支持/);
});
