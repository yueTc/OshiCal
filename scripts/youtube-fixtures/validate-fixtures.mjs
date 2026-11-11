#!/usr/bin/env node
// Read-only fixture contract checks. This script performs no network access and
// writes no files. Run it only in CI or another explicitly approved environment.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const fixtureDirectory = dirname(fileURLToPath(import.meta.url));
const expectedFixtureVersion = 1;
const channelIdPattern = /^UC[A-Za-z0-9_-]{22}$/;
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

function assertFixtureVersion(fixture, name) {
  assert.equal(
    fixture.fixtureVersion,
    expectedFixtureVersion,
    `${name} fixture version must be ${expectedFixtureVersion}`,
  );
}

function isYoutubeHost(host) {
  return host === "youtu.be" || host === "youtube.com" || host.endsWith(".youtube.com");
}

function looksLikeUrlInput(input) {
  const lower = input.toLowerCase();
  return lower.startsWith("http://")
    || lower.startsWith("https://")
    || lower.startsWith("youtube.com/")
    || lower.startsWith("www.youtube.com/")
    || lower.startsWith("m.youtube.com/")
    || lower.startsWith("music.youtube.com/")
    || lower.startsWith("youtu.be/");
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
  if (videoIdPattern.test(input)) {
    return parsedAccount("video", {
      videoId: input,
      canonicalUrl: canonicalVideoUrl(input),
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
  if (!looksLikeUrlInput(input)) {
    return parsedAccount("search", { canonicalUrl: null });
  }

  let url;
  try {
    const urlInput = input.includes("://") ? input : `https://${input}`;
    url = new URL(urlInput);
  } catch {
    throw fixtureError("INVALID_CHANNEL_ID", "unsupported account input");
  }
  const host = normalizedHost(url);
  if (!isYoutubeHost(host)) {
    // Product contract: explicit non-YouTube URLs are safe free-text searches.
    // They are never navigated to or used as an upstream request URL.
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
      return parsedAccount("search", { canonicalUrl: null });
    }
    return parsedAccount("channel", {
      channelId,
      canonicalUrl: canonicalChannelUrl(channelId),
    });
  }
  if (segments[0] === "watch") {
    const videoId = url.searchParams.get("v") ?? "";
    if (!videoIdPattern.test(videoId)) {
      return parsedAccount("search", { canonicalUrl: null });
    }
    return parsedAccount("video", {
      videoId,
      canonicalUrl: canonicalVideoUrl(videoId),
    });
  }
  if (["live", "embed", "shorts", "v"].includes(segments[0])) {
    const videoId = segments[1] ?? "";
    if (!videoIdPattern.test(videoId)) {
      return parsedAccount("search", { canonicalUrl: null });
    }
    return parsedAccount("video", {
      videoId,
      canonicalUrl: canonicalVideoUrl(videoId),
    });
  }

  return parsedAccount("search", { canonicalUrl: null });
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
  const byUrl = new Map();
  walk(post.backstageAttachment ?? {}, (value) => {
    if (!value || typeof value !== "object") {
      return;
    }
    if (typeof value.url === "string" && value.url.startsWith("http")) {
      const width = Number(value.width ?? 0);
      const height = Number(value.height ?? 0);
      const area = Number.isFinite(width * height) ? width * height : 0;
      const previous = byUrl.get(value.url);
      if (!previous || area > previous.area) {
        byUrl.set(value.url, { url: value.url, area });
      }
    }
  });
  const largest = [...byUrl.values()].sort((left, right) => right.area - left.area)[0];
  return largest ? [largest.url] : [];
}

function postSourceUrl(post, postId) {
  const relative = post.navigationEndpoint?.commandMetadata?.webCommandMetadata?.url;
  if (typeof relative === "string" && relative.length > 0) {
    return relative.startsWith("http") ? relative : `https://www.youtube.com${relative}`;
  }
  return `https://www.youtube.com/post/${postId}`;
}

function hasMembershipUnavailableSignal(initialData) {
  let matched = false;
  walk(initialData, (value) => {
    if (matched || !value || typeof value !== "object") {
      return;
    }
    const renderer = value.alertRenderer ?? value.messageRenderer ?? value.membershipUnavailableRenderer;
    if (!renderer) {
      return;
    }
    const text = [
      collectText(renderer.text),
      collectText(renderer.title),
      collectText(renderer.messageText),
    ].join(" ");
    matched = /members only|members-only|channel members only|会员专属|仅限会员|会员专享/i.test(text);
  });
  return matched;
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

  const membershipUnavailable = posts.length === 0 && hasMembershipUnavailableSignal(initialData);
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
    const recognizedChannel = typeof details.channelId === "string" && details.channelId.length > 0;
    if (!recognizedChannel && details.isLive !== false) {
      throw fixtureError(
        "YOUTUBE_LIVE_FORMAT",
        "live page has neither a video id nor a recognizable offline channel page",
      );
    }
    return {
      state: "offline",
      videoId: null,
      title: null,
      startedAt: null,
    };
  }
  const explicitLive = details.isLive === true || broadcast.isLiveNow === true;
  const explicitOffline = details.isLive === false || broadcast.isLiveNow === false;
  if (explicitLive && !explicitOffline) {
    return {
      state: "live",
      videoId,
      title: details.title ?? null,
      startedAt: broadcast.startTimestamp ?? null,
    };
  }
  if (explicitOffline) {
    // /live falling through to a completed video is offline for this probe.
    // Ended broadcasts are authoritative only from videos.list actualEndTime.
    return {
      state: "offline",
      videoId: null,
      title: null,
      startedAt: null,
    };
  }
  throw fixtureError(
    "YOUTUBE_LIVE_FORMAT",
    "live page has a video id but no explicit live/offline signal",
  );
}

function evaluateOAuthCase(testCase, requiredScopes) {
  if (
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
assertFixtureVersion(accountFixtures, "account-inputs");
for (const testCase of accountFixtures.validCases) {
  let parsed;
  try {
    parsed = parseAccountInput(testCase.input);
  } catch (error) {
    if (
      testCase.expected.kind === "search"
      && looksLikeUrlInput(testCase.input)
      && error?.code === "INVALID_CHANNEL_ID"
    ) {
      throw fixtureError(
        "PRODUCTION_CONTRACT_MISMATCH",
        `fixture expects a non-YouTube URL to become a search term, but the fixture validator rejected it: ${testCase.input}`,
      );
    }
    throw error;
  }
  assert.deepEqual(
    compact(parsed),
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
assert.deepEqual(
  compact(parseAccountInput("dQw4w9WgXcQ")),
  {
    kind: "video",
    channelId: null,
    handle: null,
    videoId: "dQw4w9WgXcQ",
    canonicalUrl: "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
  },
  "bare video id must follow production parser precedence",
);

const dataApi = await readFixture("data-api.json");
assertFixtureVersion(dataApi, "data-api");
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
assertFixtureVersion(community, "community");
for (const key of ["public", "membersOnlyAccessible", "membersOnlyUnavailable"]) {
  assert.deepEqual(
    parseCommunityFixture(community[key]),
    community[key].expected,
    `community fixture failed: ${key}`,
  );
}
assert.deepEqual(
  parseCommunityFixture({
    htmlResponse: {
      ytInitialData: {
        contents: {
          twoColumnBrowseResultsRenderer: {
            tabs: [],
          },
        },
      },
    },
  }),
  { skipped: false, skipReason: null, posts: [] },
  "an empty page without an explicit membership error must not be reported as a membership skip",
);

const livePage = await readFixture("live-page.json");
assertFixtureVersion(livePage, "live-page");
for (const key of ["live", "notLiveChannelPage", "endedBroadcastPage"]) {
  assert.deepEqual(
    mapLivePage(livePage[key]),
    livePage[key].expected,
    `live page fixture failed: ${key}`,
  );
}
const ambiguousLivePage = structuredClone(livePage.live);
delete ambiguousLivePage.playerResponse.videoDetails.isLive;
delete ambiguousLivePage.playerResponse.microformat.playerMicroformatRenderer.liveBroadcastDetails.isLiveNow;
assert.throws(
  () => mapLivePage(ambiguousLivePage),
  (error) => error?.code === "YOUTUBE_LIVE_FORMAT",
  "a live URL with no explicit live/offline signal must remain unknown",
);

const oauth = await readFixture("oauth-contract.json");
assertFixtureVersion(oauth, "oauth-contract");
for (const testCase of oauth.cases) {
  assert.equal(
    evaluateOAuthCase(testCase, oauth.requiredScopes),
    testCase.expected,
    `oauth scenario failed: ${testCase.name}`,
  );
}
assert.equal(
  evaluateOAuthCase(
    {
      hasApiKey: true,
      hasOAuth: false,
      tokenStatus: "missing",
      scopes: [],
      capability: "live_chat",
      callbackState: "tampered-state",
      expectedState: "state-001",
    },
    oauth.requiredScopes,
  ),
  "reject_callback",
  "state mismatch must be rejected before checking whether OAuth is present",
);

const legacy = await readFixture("legacy-bilibili.json");
assertFixtureVersion(legacy, "legacy-bilibili");
for (const table of ["streamers", "linkedAccounts"]) {
  assert.equal(
    legacy.sourceSchema[table].includes("platform"),
    false,
    `legacy ${table} fixture must not pre-contain platform`,
  );
  for (const expected of legacy.expected[table]) {
    const source = legacy.rows[table].find((row) => row.id === expected.id);
    assert.ok(source, `legacy ${table} row ${expected.id} is missing`);
    assert.deepEqual(
      Object.keys(source).sort(),
      [...legacy.sourceSchema[table]].sort(),
      `legacy ${table} row ${expected.id} does not match its declared source schema`,
    );
    assert.equal(source.mid, expected.mid, `legacy ${table} row ${expected.id} mid changed`);
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
