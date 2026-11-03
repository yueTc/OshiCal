#!/usr/bin/env node
import fs from "node:fs";

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error("用法: node scripts/set-version.mjs <semver>");
  process.exit(1);
}

function updateJson(path, update) {
  const value = JSON.parse(fs.readFileSync(path, "utf8"));
  update(value);
  fs.writeFileSync(path, JSON.stringify(value, null, 2) + "\n", "utf8");
}

updateJson("package.json", (value) => {
  value.version = version;
});
updateJson("package-lock.json", (value) => {
  value.version = version;
  if (value.packages?.[""]) {
    value.packages[""].version = version;
  }
});
updateJson("src-tauri/tauri.conf.json", (value) => {
  value.version = version;
});

const cargoPath = "Cargo.toml";
const cargoLines = fs.readFileSync(cargoPath, "utf8").split(/\r?\n/);
let inWorkspacePackage = false;
let cargoUpdated = false;
for (let index = 0; index < cargoLines.length; index += 1) {
  const line = cargoLines[index];
  if (line.trim() === "[workspace.package]") {
    inWorkspacePackage = true;
    continue;
  }
  if (inWorkspacePackage && line.startsWith("[") && line.trim() !== "[workspace.package]") {
    break;
  }
  if (inWorkspacePackage && line.startsWith("version = ")) {
    cargoLines[index] = 'version = "' + version + '"';
    cargoUpdated = true;
    break;
  }
}
if (!cargoUpdated) {
  throw new Error("未在 Cargo.toml 的 [workspace.package] 中找到 version");
}
fs.writeFileSync(cargoPath, cargoLines.join("\n"), "utf8");

const workspaceCrates = new Set(["oshical-core", "oshical-desktop", "oshical-server"]);
const lockPath = "Cargo.lock";
const lock = fs.readFileSync(lockPath, "utf8").replace(/\r\n/g, "\n");
const blocks = lock.split(/(?=\[\[package\]\]\n)/);
let lockUpdated = 0;
for (let index = 0; index < blocks.length; index += 1) {
  const name = blocks[index].match(/^name = "([^"]+)"$/m)?.[1];
  if (!name || !workspaceCrates.has(name)) {
    continue;
  }
  blocks[index] = blocks[index].replace(/^version = "[^"]+"$/m, 'version = "' + version + '"');
  lockUpdated += 1;
}
if (lockUpdated !== workspaceCrates.size) {
  throw new Error("Cargo.lock 工作区包版本更新不完整: " + lockUpdated + "/" + workspaceCrates.size);
}
fs.writeFileSync(lockPath, blocks.join(""), "utf8");

console.log("版本已更新为 " + version);
