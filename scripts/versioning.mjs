export function nextBetaVersion(current) {
  const match = current.match(/^(\d+)\.(\d+)\.(\d+)(?:-beta\.(\d+))?$/);
  if (!match) {
    throw new Error("next-beta 只支持正式版或 X.Y.Z-beta.N 版本，当前为 " + current);
  }
  const [, major, minor, patch, beta] = match;
  if (beta) {
    return `${major}.${minor}.${patch}-beta.${Number(beta) + 1}`;
  }
  return `${major}.${minor}.${Number(patch) + 1}-beta.1`;
}

export function promoteStableVersion(current) {
  const match = current.match(/^(\d+\.\d+\.\d+)-beta\.\d+$/);
  if (!match) {
    throw new Error("promote-stable 只支持 X.Y.Z-beta.N 版本，当前为 " + current);
  }
  return match[1];
}
