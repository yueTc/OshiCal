#!/usr/bin/env node
// Read-only fixture contract checks. This script performs no network access and
// writes no files. Run it only in CI or another explicitly approved environment.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const fixtureDirectory = dirname(fileURLToPath(import.meta.url));
const channelIdPattern = /^UC[A-Za-z0-9_-]{20,}$/;
const handlePattern = /^@[A-Za-z0-9._-]{3,30}$/;
const videoIdPattern = /^[A-Za-z0-9_-]{11}$/;

async function readFixture(name) {
  const raw = await readFile(join(fixtureDirectory, name), "utf8");
  return JSON.parse(raw);
}

function fixtureError(code, message) {
  const error = new Error(message);
  error.code = code;
  return error;
}

function normalizedHost(url) {
  return url.hostname.toLowerCase().replace(/^(www\.|m\.)/, "");
}

function canonicalChannelUrl(channelId) {
  return `https://www.youtube.com/channel/${channelId}`;
}

function canonicalHandleUrl(handle) {
  return `https://www.youtube.com/${handle}`;
}

function canonicalVideoUrl(videoId) {
  return `https://www.youtube.com/watch?v=${videoId}`;
}

function parsedAccount(kind, values) {
  return {
    kind,
    channelId: values.channelId ?? null,
    handle: values.handle ?? null,
    videoId: values.videoId ?? null,
    canonicalUrl: values.canonicalUrl,
  };
}

export function parseAccountInput(rawInput) {
  const input = String(rawInput ?? "").trim();
  if (!input) {
    throw fixtureError("EMPTY_INPUT", "account input is empty");
  }
  if (channelIdPattern.test(input)) {
    return parsedAccount("channel", {
      channelId: input,
      canonicalUrl: canonicalChannelUrl(input),
    });
  }
  if (handlePattern.test(input)) {
    return parsedAccount("handle", {
      handle: input,
      canonicalUrl: canonicalHandleUrl(input),
    });
  }
  if (input.startsWith("@")) {
    throw fixtureError("INVALID_HANDLE", "invalid YouTube handle");
  }
  if (!input.includes("://") && !input.startsWith("www.") && !input.startsWith("m.youtube.com/")) {
    return parsedAccount("search", { canonicalUrl: null });
  }

  let url;
  try {
    const urlInput = input.startsWith("www.") || input.startsWith("m.youtube.com/") || input.startsWith("youtube.com/")
      ? `https://${input}`
      : input;
    url = new URL(urlInput);
  } catch {
    throw fixtureError("INVALID_CHANNEL_ID", "unsupported account input");
  }
  const host = normalizedHost(url);
  if (!["youtube.com", "youtu.be", "youtube-nocookie.com"].includes(host)) {
    return parsedAccount("search", { canonicalUrl: null });
  }

  const path = decodeURIComponent(url.pathname);
  if (host === "youtu.be") {
    const videoId = path.split("/").filter(Boolean)[0] ?? "";
    if (!videoIdPattern.test(videoId)) {
      throw fixtureError("INVALID_VIDEO_ID", "invalid youtu.be video id");
    }
    return parsedAccount("video", {
      videoId,
      canonicalUrl: canonicalVideoUrl(videoId),
    });
  }

  if (path.startsWith("/@")) {
    const handle = path.split("/").filter(Boolean)[0] ?? "";
    if (!handlePattern.test(handle)) {
      throw fixtureError("INVALID_HANDLE", "invalid YouTube handle");
    }
    return parsedAccount("handle", {
      handle,
      canonicalUrl: canonicalHandleUrl(handle),
    });
  }

  const segments = path.split("/").filter(Boolean);
  if (segments[0] === "channel") {
    const channelId = segments[1] ?? "";
    if (!channelIdPattern.test(channelId)) {
      throw fixtureError("INVALID_CHANNEL_ID", "invalid YouTube channel id");
    }
    return parsedAccount("channel", {
      channelId,
      canonicalUrl: canonicalChannelUrl(channelId),
    });
  }
  if (segments[0] === "watch") {
    const videoId = url.searchParams.get("v") ?? "";
    if (!videoIdPattern.test(videoId)) {
      throw fixtureError("INVALID_VIDEO_ID", "invalid watch video id");
    }
    return parsedAccount("video", {
      videoId,
      canonicalUrl: canonicalVideoUrl(videoId),
    });
  }
  if (segments[0] === "live") {
    const videoId = segments[1] ?? "";
    if (!videoIdPattern.test(videoId)) {
      throw fixtureError("INVALID_VIDEO_ID", "invalid live video id");
    }
    return parsedAccount("video", {
      videoId,
      canonicalUrl: canonicalVideoUrl(videoId),
    });
  }

  throw fixtureError("INVALID_CHANNEL_ID", "unsupported YouTube URL");
}

function bestThumbnail(thumbnails) {
  return thumbnails?.high?.url ?? thumbnails?.medium?.url ?? thumbnails?.default?.url ?? "";
}

function mapSearchItem(item) {
  const channelId = item.id?.channelId ?? item.snippet?.channelId ?? "";
  return {
    channelId,
    title: item.snippet?.title ?? "",
    avatarUrl: bestThumbnail(item.snippet?.thumbnails),
    profileUrl: canonicalChannelUrl(channelId),
  };
}

function mapChannel(item) {
  return {
    channelId: item.id ?? "",
    handle: item.snippet?.customUrl ?? null,
    title: item.snippet?.title ?? "",
    uploadsPlaylistId: item.contentDetails?.relatedPlaylists?.uploads ?? null,
    subscriberCount: Number(item.statistics?.subscriberCount ?? 0),
    hiddenSubscriberCount: Boolean(item.statistics?.hiddenSubscriberCount),
  };
}

function numberOrNull(value) {
  if (value === undefined || value === null || value === "") {
    return null;
  }
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function mapVideoItem(item) {
  const details = item.liveStreamingDetails ?? {};
  let state = "none";
  if (details.actualEndTime) {
    state = "ended";
  } else if (item.snippet?.liveBroadcastContent === "upcoming") {
    state = "upcoming";
  } else if (item.snippet?.liveBroadcastContent === "live") {
    state = "live";
  }
  return {
    state,
    scheduledStartTime: details.scheduledStartTime ?? null,
    actualStartTime: details.actualStartTime ?? null,
    actualEndTime: details.actualEndTime ?? null,
    concurrentViewers: numberOrNull(details.concurrentViewers),
  };
}

function collectText(node) {
  if (!node) {
    return "";
  }
  if (typeof node.simpleText === "string") {
    return node.simpleText;
  }
  if (Array.isArray(node.runs)) {
    return node.runs.map((run) => run.text ?? "").join("");
  }
  return "";
}

function walk(value, visit) {
  visit(value);
  if (Array.isArray(value)) {
    for (const item of value) {
      walk(item, visit);
    }
  } else if (value && typeof value === "object") {
    for (const child of Object.values(value)) {
      walk(child, visit);
    }
  }
}

function collectImageUrls(post) {
  const urls = [];
  walk(post.backstageAttachment ?? {}, (value) => {
    if (!value || typeof value !== "object") {
      return;
    }
    if (typeof value.url === "string" && value.url.startsWith("http")) {
      urls.push(value.url);
    }
  });
  return [...new Set(urls)].slice(-1);
}

function postSourceUrl(post, postId) {
  const relative = post.navigationEndpoint?.commandMetadata?.webCommandMetadata?.url;
  if (typeof relative === "string" && relative.length > 0) {
    return relative.startsWith("http") ? relative : `https://www.youtube.com${relative}`;
  }
  return `https://www.youtube.com/post/${postId}`;
}

function parseCommunityFixture(entry) {
  const initialData = entry.htmlResponse?.ytInitialData ?? {};
  const posts = [];
  walk(initialData, (value) => {
    if (!value || typeof value !== "object") {
      return;
    }
    const post = value.backstagePostRenderer ?? value.postRenderer;
    if (!post) {
      return;
    }
    const postId = post.postId ?? "";
    posts.push({
      postId,
      authorName: collectText(post.authorText),
      publishedText: collectText(post.publishedTimeText),
      text: collectText(post.contentText),
      images: collectImageUrls(post),
      sourceUrl: postSourceUrl(post, postId),
      membershipOnly: Boolean(post.sponsorsOnlyBadge ?? post.membersOnlyBadge),
    });
  });

  const serialized = JSON.stringify(initialData);
  const membershipUnavailable =
    posts.length === 0 && /members only|channel members only|会员专属/i.test(serialized);
  return {
    skipped: membershipUnavailable,
    skipReason: membershipUnavailable ? "membership_session_unavailable" : null,
    posts,
  };
}

function mapLivePage(entry) {
  const finalUrl = new URL(entry.finalUrl);
  const details = entry.playerResponse?.videoDetails ?? {};
  const microformat = entry.playerResponse?.microformat?.playerMicroformatRenderer;
  const broadcast = microformat?.liveBroadcastDetails ?? {};
  const videoId = finalUrl.searchParams.get("v");

  if (!videoId) {
    return {
      state: "offline",
      videoId: null,
      title: null,
      startedAt: null,
    };
  }
  if (details.isLive === true && broadcast.isLiveNow !== false) {
    return {
      state: "live",
      videoId,
      title: details.title ?? null,
      startedAt: broadcast.startTimestamp ?? null,
    };
  }
  // /live falling through to a completed video is indistinguishable from a
  // channel page in the production probe. Ended broadcasts are authoritative
  // only from videos.list with actualEndTime.
  return {
    state: "offline",
    videoId: null,
    title: null,
    startedAt: null,
  };
}

function evaluateOAuthCase(testCase, requiredScopes) {
  if (
    testCase.hasOAuth &&
    testCase.callbackState !== null &&
    testCase.callbackState !== testCase.expectedState
  ) {
    return "reject_callback";
  }
  if (!testCase.hasOAuth || testCase.tokenStatus !== "valid") {
    return "reauth_required";
  }
  const hasRequiredScope = requiredScopes.every((scope) => testCase.scopes.includes(scope));
  return hasRequiredScope ? "allowed" : "reauth_required";
}

function compact(actual) {
  return JSON.parse(JSON.stringify(actual));
}

const accountFixtures = await readFixture("account-inputs.json");
for (const testCase of accountFixtures.validCases) {
  assert.deepEqual(
    compact(parseAccountInput(testCase.input)),
    testCase.expected,
    `account input failed: ${testCase.input}`,
  );
}
for (const testCase of accountFixtures.invalidCases) {
  assert.throws(
    () => parseAccountInput(testCase.input),
    (error) => error?.code === testCase.expectedError,
    `invalid input did not fail as ${testCase.expectedError}: ${testCase.input}`,
  );
}

const dataApi = await readFixture("data-api.json");
assert.match(dataApi.search.items[0].id.channelId, channelIdPattern);
assert.match(dataApi.channels.items[0].id, channelIdPattern);
assert.deepEqual(mapSearchItem(dataApi.search.items[0]), dataApi.expected.searchFirst);
assert.deepEqual(mapChannel(dataApi.channels.items[0]), dataApi.expected.channel);
for (const item of dataApi.videos.items) {
  assert.match(item.id, videoIdPattern, `invalid fixture video id: ${item.id}`);
  assert.deepEqual(
    mapVideoItem(item),
    dataApi.expected.videos[item.id],
    `video mapping failed: ${item.id}`,
  );
}

const community = await readFixture("community.json");
for (const key of ["public", "membersOnlyAccessible", "membersOnlyUnavailable"]) {
  assert.deepEqual(
    parseCommunityFixture(community[key]),
    community[key].expected,
    `community fixture failed: ${key}`,
  );
}

const livePage = await readFixture("live-page.json");
for (const key of ["live", "notLiveChannelPage", "endedBroadcastPage"]) {
  assert.deepEqual(
    mapLivePage(livePage[key]),
    livePage[key].expected,
    `live page fixture failed: ${key}`,
  );
}

const oauth = await readFixture("oauth-contract.json");
for (const testCase of oauth.cases) {
  assert.equal(
    evaluateOAuthCase(testCase, oauth.requiredScopes),
    testCase.expected,
    `oauth scenario failed: ${testCase.name}`,
  );
}

const legacy = await readFixture("legacy-bilibili.json");
for (const table of ["streamers", "linkedAccounts"]) {
  assert.equal(
    legacy.sourceSchema[table].includes("platform"),
    false,
    `legacy ${table} fixture must not pre-contain platform`,
  );
  for (const expected of legacy.expected[table]) {
    const source = legacy.rows[table].find((row) => row.id === expected.id);
    assert.ok(source, `legacy ${table} row ${expected.id} is missing`);
    assert.equal(expected.platform, "bilibili", `legacy ${table} platform default`);
    assert.equal(expected.handle, null, `legacy ${table} handle default`);
  }
}
for (const expected of legacy.expected.streamers) {
  assert.equal(expected.profile_url, null);
  assert.equal(expected.uploads_playlist_id, null);
}
for (const expected of legacy.expected.linkedAccounts) {
  assert.equal(expected.profile_url, null);
}
console.log(
  [
    `account-inputs: ${accountFixtures.validCases.length + accountFixtures.invalidCases.length}`,
    `data-api: ${1 + 1 + dataApi.videos.items.length}`,
    `community: 3`,
    `live-page: 3`,
    `oauth: ${oauth.cases.length}`,
    `legacy-bilibili: ${legacy.expected.streamers.length + legacy.expected.linkedAccounts.length}`,
  ].join(" | "),
);
