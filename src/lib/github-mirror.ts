const CLIENT_GITHUB_MIRROR_KEY = "oshical.githubMirrorUrl";

export function normalizeGithubMirrorUrl(value: string): string {
  const trimmed = value.trim();
  if (!trimmed) {
    return "";
  }
  const url = new URL(trimmed);
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("GitHub 镜像地址必须使用 http 或 https");
  }
  const normalized = url.toString();
  return normalized.endsWith("/") ? normalized : `${normalized}/`;
}

export function readClientGithubMirror(): string {
  try {
    return localStorage.getItem(CLIENT_GITHUB_MIRROR_KEY) ?? "";
  } catch {
    return "";
  }
}

export function saveClientGithubMirror(value: string): string {
  const normalized = normalizeGithubMirrorUrl(value);
  try {
    if (normalized) {
      localStorage.setItem(CLIENT_GITHUB_MIRROR_KEY, normalized);
    } else {
      localStorage.removeItem(CLIENT_GITHUB_MIRROR_KEY);
    }
  } catch {
    // The in-memory setting remains active for this page session.
  }
  return normalized;
}
