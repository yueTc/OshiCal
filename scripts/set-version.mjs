#!/usr/bin/env node
import fs from "node:fs";
import { nextBetaVersion, promoteStableVersion } from "./versioning.mjs";

const rawTarget = process.argv[2];
const lifecycleMode = process.env.npm_lifecycle_event === "version:next-beta"
  ? "next-beta"
  : process.env.npm_lifecycle_event === "version:promote-stable"
    ? "promote-stable"
    : undefined;
const requestedVersion = process.argv[3] ?? lifecycleMode;
const target = rawTarget === "client" ? "desktop" : rawTarget;
const targets = new Set(["desktop", "server", "core"]);
const versionModes = new Set(["next-beta", "promote-stable"]);

if (!targets.has(target) || !requestedVersion) {
  console.error("用法: node scripts/set-version.mjs <desktop|server|core> <semver|next-beta|promote-stable>");
  console.error("或: npm run version:next-beta|version:promote-stable -- <desktop|server>");
  process.exit(1);
}

function updateJson(path, update) {
  const value = JSON.parse(fs.readFileSync(path, "utf8"));
  update(value);
  fs.writeFileSync(path, JSON.stringify(value, null, 2) + "\n", "utf8");
}

function updateCargoPackageVersion(path) {
  const lines = fs.readFileSync(path, "utf8").split(/\r?\n/);
  let inPackage = false;
  let updated = false;
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.trim() === "[package]") {
      inPackage = true;
      continue;
    }
    if (inPackage && line.startsWith("[")) {
      break;
    }
    if (inPackage && (line.startsWith("version = ") || line.startsWith("version.workspace = "))) {
      lines[index] = 'version = "' + version + '"';
      updated = true;
      break;
    }
  }
  if (!updated) {
    throw new Error("未在 [package] 中找到版本字段: " + path);
  }
  fs.writeFileSync(path, lines.join("\n"), "utf8");
}

function readJsonVersion(path) {
  return JSON.parse(fs.readFileSync(path, "utf8")).version;
}

function readCargoPackageVersion(path) {
  const lines = fs.readFileSync(path, "utf8").split(/\r?\n/);
  let inPackage = false;
  for (const line of lines) {
    if (line.trim() === "[package]") {
      inPackage = true;
      continue;
    }
    if (inPackage && line.startsWith("[")) {
      break;
    }
    if (inPackage && line.startsWith("version = ")) {
      return line.match(/^version = "([^"]+)"$/)?.[1];
    }
  }
  throw new Error("未在 [package] 中找到版本字段: " + path);
}

function readWorkspaceVersion(path) {
  const lines = fs.readFileSync(path, "utf8").split(/\r?\n/);
  let inWorkspacePackage = false;
  for (const line of lines) {
    if (line.trim() === "[workspace.package]") {
      inWorkspacePackage = true;
      continue;
    }
    if (inWorkspacePackage && line.startsWith("[")) {
      break;
    }
    if (inWorkspacePackage && line.startsWith("version = ")) {
      return line.match(/^version = "([^"]+)"$/)?.[1];
    }
  }
  throw new Error("未在 [workspace.package] 中找到版本字段");
}

function currentVersion() {
  if (target === "desktop") {
    return readJsonVersion("package.json");
  }
  if (target === "server") {
    return readCargoPackageVersion("server/Cargo.toml");
  }
  return readWorkspaceVersion("Cargo.toml");
}

function updateWorkspaceVersion(path) {
  const lines = fs.readFileSync(path, "utf8").split(/\r?\n/);
  let inWorkspacePackage = false;
  let updated = false;
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.trim() === "[workspace.package]") {
      inWorkspacePackage = true;
      continue;
    }
    if (inWorkspacePackage && line.startsWith("[")) {
      break;
    }
    if (inWorkspacePackage && line.startsWith("version = ")) {
      lines[index] = 'version = "' + version + '"';
      updated = true;
      break;
    }
  }
  if (!updated) {
    throw new Error("未在 [workspace.package] 中找到版本字段");
  }
  fs.writeFileSync(path, lines.join("\n"), "utf8");
}

function updateLockPackageVersion(names) {
  const lockPath = "Cargo.lock";
  const lock = fs.readFileSync(lockPath, "utf8").replace(/\r\n/g, "\n");
  const blocks = lock.split(/(?=\[\[package\]\]\n)/);
  let updated = 0;
  for (let index = 0; index < blocks.length; index += 1) {
    const name = blocks[index].match(/^name = "([^"]+)"$/m)?.[1];
    if (!name || !names.has(name)) {
      continue;
    }
    blocks[index] = blocks[index].replace(/^version = "[^"]+"$/m, 'version = "' + version + '"');
    updated += 1;
  }
  if (updated !== names.size) {
    throw new Error("Cargo.lock 包版本更新不完整: " + updated + "/" + names.size);
  }
  fs.writeFileSync(lockPath, blocks.join(""), "utf8");
}

let version = requestedVersion;
if (versionModes.has(requestedVersion)) {
  if (target === "core") {
    console.error("版本节奏命令只适用于 desktop / server");
    process.exit(1);
  }
  const current = currentVersion();
  try {
    version = requestedVersion === "next-beta"
      ? nextBetaVersion(current)
      : promoteStableVersion(current);
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
  console.log(`版本节奏: ${current} -> ${version}`);
} else if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  console.error("版本号必须是 SemVer，例如 0.3.21 或 0.3.21-beta.1");
  process.exit(1);
}

if (target === "desktop") {
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
  updateCargoPackageVersion("src-tauri/Cargo.toml");
  updateLockPackageVersion(new Set(["oshical-desktop"]));
  console.log("桌面端版本已更新为 " + version);
} else if (target === "server") {
  updateCargoPackageVersion("server/Cargo.toml");
  updateLockPackageVersion(new Set(["oshical-server"]));
  console.log("服务端版本已更新为 " + version);
} else {
  updateWorkspaceVersion("Cargo.toml");
  updateLockPackageVersion(new Set(["oshical-core"]));
  console.log("核心库版本已更新为 " + version);
}
