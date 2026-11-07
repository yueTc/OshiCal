import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { isTauri } from "./api";
import { isMobileShell } from "./platform";

export async function readAutostart(): Promise<boolean> {
  if (!isTauri || isMobileShell) {
    return false;
  }
  try {
    return await isEnabled();
  } catch {
    return false;
  }
}

export async function applyAutostart(enabled: boolean): Promise<void> {
  if (!isTauri || isMobileShell) {
    return;
  }
  if (enabled) {
    await enable();
  } else {
    await disable();
  }
}
