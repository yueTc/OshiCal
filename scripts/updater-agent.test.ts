import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const source = readFileSync(new URL("./oshical-updater-agent.sh", import.meta.url), "utf8");

test("updater write helpers consume piped data from a separate file descriptor", () => {
  const replaceStart = source.indexOf("safe_replace_file()");
  const appendStart = source.indexOf("safe_append_file()");
  const removeStart = source.indexOf("safe_remove_file()");
  assert.ok(replaceStart >= 0 && appendStart > replaceStart && removeStart > appendStart);

  const replaceBlock = source.slice(replaceStart, appendStart);
  const appendBlock = source.slice(appendStart, removeStart);
  for (const block of [replaceBlock, appendBlock]) {
    assert.match(block, /python3 - "\$target" 3<&0 <<'PY'/);
    assert.match(block, /os\.read\(3, 65536\)/);
    assert.doesNotMatch(block, /sys\.stdin\.buffer\.read\(/);
  }
});