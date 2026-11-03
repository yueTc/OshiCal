export function httpBaseFromWebSocketUrl(value: string): string {
  const url = new URL(value);
  const protocol = url.protocol === "wss:"
    ? "https:"
    : url.protocol === "ws:"
      ? "http:"
      : url.protocol;
  const path = url.pathname.replace(/\/api\/ws\/?$/, "");
  return `${protocol}//${url.host}${path === "/" ? "" : path}`;
}
