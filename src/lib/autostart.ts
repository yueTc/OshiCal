import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { isTauri } from "./api";

export async function readAutostart(): Promise<boolean> {
  if (!isTauri) {
    return false;
  }
  try {
    return await isEnabled();
  } catch {
    return false;
  }
}

export async function applyAutostart(enabled: boolean): Promise<void> {
  if (!isTauri) {
    return;
  }
  if (enabled) {
    await enable();
  } else {
    await disable();
  }
}
