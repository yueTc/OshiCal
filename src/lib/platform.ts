// 安卓/iOS 上没有托盘、自启、应用内更新和窗口控制，前端按此隐藏桌面外壳。
export function detectMobileShell(userAgent: string): boolean {
  return /Android|iPhone|iPad|iPod/i.test(userAgent);
}

export const isMobileShell =
  typeof navigator !== "undefined" && detectMobileShell(navigator.userAgent);

export function resolveDesktopShell(tauri: boolean, mobile: boolean): boolean {
  return tauri && !mobile;
}
