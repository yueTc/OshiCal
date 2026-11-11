// CI only: run exactly the regression suite against the pre-fix shipped source.
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
const dir = mkdtempSync(join(tmpdir(), "oshical-live-baseline-"));
try {
  const path = join(dir, "live-schedule.ts");
  writeFileSync(path, execFileSync("git", ["show", "fa78d8c44cdca5dfbb81bdc70fc96689a4c35312:src/lib/live-schedule.ts"]));
  const result = spawnSync(process.execPath, ["--experimental-strip-types", "--test", "--test-reporter=tap", "scripts/live-schedule-regression.test.ts"], {
    encoding: "utf8", env: { ...process.env, OSHICAL_LIVE_SCHEDULE_MODULE: path },
  });
  const output = result.stdout + result.stderr;
  process.stdout.write(output);
  if (result.status !== 1 || !/# fail [1-9]/.test(output) || !/AssertionError/.test(output)) {
    console.error("::error title=Pre-fix regression diagnostics::" + output.slice(-12000).replaceAll("%", "%25").replaceAll("\r", "%0D").replaceAll("\n", "%0A"));
  }
  assert.equal(result.status, 1, "pre-fix source must reproduce the regression");
  assert.match(output, /legacy yesterday-only cards must not be marked live/);
  assert.match(output, /AssertionError/);
  assert.match(output, /# fail [1-9]/);
  console.log("Confirmed original code fails the same retained regression suite.");
  console.log("::notice title=Pre-fix regression evidence::Same retained suite reproduced " + output.match(/# fail (\d+)/)[1] + " assertion failures on shipped pre-fix source.");
} finally { rmSync(dir, { recursive: true, force: true }); }
