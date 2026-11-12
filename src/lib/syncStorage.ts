import type {
  LiveSession,
  LiveStatus,
  Platform,
  Schedule,
  ScheduleSource,
  SourceSuppression,
  Streamer,
  SyncBootstrap,
  SyncChange,
  WeeklyScheduleException,
  WeeklyScheduleRule,
} from "../types";

export const WEB_SYNC_CACHE_SCHEMA_VERSION = 3;
const WEB_SYNC_DB_NAME = "oshical.web-sync.v3";
const WEB_SYNC_DB_VERSION = 1;
const META_STORE = "sync_meta";
const ENTITY_STORE = "sync_entity_cache";
const SERVER_INDEX = "serverKey";
const SERVER_SETTINGS_ENTITY_ID = "server_settings";

export interface WebSyncCacheMeta {
  serverKey: string;
  schemaVersion: number;
  feedId?: string;
  cursorSeq: number;
  latestSeq: number;
  minRetainedSeq: number;
  snapshotRevision: number;
  ackedSeq: number;
  lastSyncAt: string | null;
  initialized: boolean;
  updatedAt: string;
}

export interface WebSyncEntityRecord {
  cacheKey: string;
  serverKey: string;
  entityType: string;
  entityId: string;
  entityRevision: number;
  operation?: "upsert" | "delete";
  seq?: number;
  payload: unknown;
  updatedAt: string;
}

export interface WebSyncCache {
  meta: WebSyncCacheMeta;
  entities: WebSyncEntityRecord[];
  snapshot: SyncBootstrap;
}

export interface WebSyncCacheLoadResult {
  available: boolean;
  cache: WebSyncCache | null;
}

export interface WebSyncCacheMetaInput {
  feedId?: string;
  cursorSeq: number;
  latestSeq: number;
  minRetainedSeq: number;
  snapshotRevision: number;
  ackedSeq: number;
  lastSyncAt: string | null;
}

export type WebSyncCacheWriteStatus = "persisted" | "unavailable" | "failed";

export interface WebSyncCacheWriteResult {
  status: WebSyncCacheWriteStatus;
  error: Error | null;
}

let databasePromise: Promise<IDBDatabase | null> | null = null;
const loadedCacheFeedIds = new Map<string, string>();
const loadedCacheServerKeys = new Set<string>();

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function nonNegativeInteger(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value)
    ? Math.max(0, Math.trunc(value))
    : fallback;
}

function stringValue(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

export function normalizeWebSyncServerKey(baseUrl: string): string {
  const trimmed = baseUrl.trim();
  if (!trimmed) {
    return "";
  }
  try {
    const url = new URL(trimmed);
    url.hash = "";
    url.search = "";
    return url.toString().replace(/\/+$/, "");
  } catch {
    return trimmed.replace(/\/+$/, "");
  }
}

export function webSyncEntityCacheKey(serverKey: string, entityType: string, entityId: string): string {
  return `${serverKey}\u0000${entityType}\u0000${entityId}`;
}

export function loadedWebSyncFeedId(baseUrl: string): string {
  return loadedCacheFeedIds.get(normalizeWebSyncServerKey(baseUrl)) ?? "";
}

export function webSyncCacheWasLoaded(baseUrl: string): boolean {
  return loadedCacheServerKeys.has(normalizeWebSyncServerKey(baseUrl));
}

function payloadId(payload: unknown, fallbackKey = "id"): string | null {
  if (!isRecord(payload)) {
    return null;
  }
  const value = payload[fallbackKey] ?? payload.id;
  if (typeof value === "string" && value.trim()) {
    return value.trim();
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    return String(Math.trunc(value));
  }
  return null;
}

function payloadRevision(payload: unknown): number {
  return isRecord(payload) ? nonNegativeInteger(payload.revision) : 0;
}

function idValue(value: unknown): string | null {
  if (typeof value === "string" && value.trim()) {
    return value.trim();
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    return String(Math.trunc(value));
  }
  return null;
}

function isLinkedAccountEntityType(entityType: string): boolean {
  return entityType === "link" || entityType === "linked_account";
}

type SyncPlatform = Platform;

function platformValue(value: unknown): SyncPlatform {
  return value === "youtube" ? "youtube" : "bilibili";
}

function linkedAccountKey(platform: unknown, mid: unknown): string | null {
  const normalizedMid = idValue(mid);
  return normalizedMid ? `${platformValue(platform)}\u0000${normalizedMid}` : null;
}

function linkedAccountIdentity(
  streamerId: unknown,
  mid: unknown,
  platform: unknown = "bilibili",
): { key: string; streamerId: string; mid: string; platform: SyncPlatform } | null {
  const normalizedStreamerId = idValue(streamerId);
  const normalizedMid = idValue(mid);
  if (!normalizedStreamerId || !normalizedMid) {
    return null;
  }
  const normalizedPlatform = platformValue(platform);
  return {
    key: `${normalizedStreamerId}\u0000${normalizedPlatform}\u0000${normalizedMid}`,
    streamerId: normalizedStreamerId,
    mid: normalizedMid,
    platform: normalizedPlatform,
  };
}

function linkedAccountIdentityFromPayload(payload: unknown) {
  if (!isRecord(payload)) {
    return null;
  }
  return linkedAccountIdentity(
    payload.streamerId ?? payload.streamer_id,
    payload.mid,
    payload.platform,
  );
}

function linkedAccountIdentityFromEntityId(entityId: string) {
  const parts = entityId.split(":");
  if (parts.length === 2) {
    // Historical link tombstones used `streamerId:mid`; those IDs predate
    // platform-aware entities and can only be interpreted as Bilibili.
    return linkedAccountIdentity(parts[0], parts[1], "bilibili");
  }
  if (parts.length >= 3) {
    const platform = parts[1].trim().toLowerCase();
    if (platform !== "bilibili" && platform !== "youtube") {
      return null;
    }
    return linkedAccountIdentity(parts[0], parts.slice(2).join(":"), platform);
  }
  return null;
}

function normalizedLinkedAccount(payload: Record<string, unknown>): Record<string, unknown> {
  return {
    ...(payload.id === undefined ? {} : { id: payload.id }),
    ...(payload.revision === undefined ? {} : { revision: payload.revision }),
    platform: platformValue(payload.platform),
    mid: idValue(payload.mid) ?? stringValue(payload.mid),
    name: stringValue(payload.name),
    avatarUrl: stringValue(payload.avatarUrl ?? payload.avatar_url),
    handle: stringValue(payload.handle) || null,
    profileUrl: stringValue(payload.profileUrl ?? payload.profile_url) || null,
    createdAt: stringValue(payload.createdAt ?? payload.created_at),
  };
}

function normalizedSubscription(payload: Record<string, unknown>): Record<string, unknown> {
  return {
    ...payload,
    platform: platformValue(payload.platform),
    handle: stringValue(payload.handle) || null,
    profileUrl: stringValue(payload.profileUrl ?? payload.profile_url) || null,
    uploadsPlaylistId: stringValue(payload.uploadsPlaylistId ?? payload.uploads_playlist_id) || null,
    liveVideoId: stringValue(payload.liveVideoId ?? payload.live_video_id) || null,
  };
}

function normalizedEntityPayload(entityType: string, payload: unknown): unknown {
  if (!isRecord(payload)) {
    return payload;
  }
  if (entityType === "subscription") {
    return normalizedSubscription(payload);
  }
  if (isLinkedAccountEntityType(entityType)) {
    return normalizedLinkedAccount(payload);
  }
  if (
    entityType === "live_status"
    || entityType === "schedule_source"
    || entityType === "source_suppression"
  ) {
    return { ...payload, platform: platformValue(payload.platform) };
  }
  return payload;
}

interface EntityVersion {
  revision: number;
  updatedAt: string;
  seq?: number;
}

interface LinkedAccountProjection {
  upserts: Map<
    string,
    {
      identity: { key: string; streamerId: string; mid: string; platform: SyncPlatform };
      payload: Record<string, unknown>;
      version: EntityVersion;
    }
  >;
  tombstones: Set<string>;
  tombstoneVersions: Map<string, EntityVersion>;
  legacyTombstoneIds: Set<string>;
  legacyTombstoneVersions: Map<string, EntityVersion>;
}

function entityVersion(record: WebSyncEntityRecord): EntityVersion {
  return {
    revision: nonNegativeInteger(record.entityRevision),
    updatedAt: record.updatedAt,
    ...(record.seq === undefined ? {} : { seq: nonNegativeInteger(record.seq) }),
  };
}

function versionIsNewer(candidate: EntityVersion, current?: EntityVersion): boolean {
  if (!current) {
    return true;
  }
  // The sync sequence is the authoritative order between an incremental delete
  // and a later re-add, even when both rows reuse revision 1.
  if (candidate.seq !== undefined || current.seq !== undefined) {
    if (candidate.seq === undefined) {
      return false;
    }
    if (current.seq === undefined) {
      return true;
    }
    if (candidate.seq !== current.seq) {
      return candidate.seq > current.seq;
    }
  }
  if (candidate.revision !== current.revision) {
    return candidate.revision > current.revision;
  }
  return candidate.updatedAt > current.updatedAt;
}

function linkedAccountProjection(cache: Pick<WebSyncCache, "entities">): LinkedAccountProjection {
  const upserts = new Map<
    string,
    {
      identity: { key: string; streamerId: string; mid: string; platform: SyncPlatform };
      payload: Record<string, unknown>;
      version: EntityVersion;
    }
  >();
  const tombstones = new Set<string>();
  const tombstoneVersions = new Map<string, EntityVersion>();
  const legacyTombstoneIds = new Set<string>();
  const legacyTombstoneVersions = new Map<string, EntityVersion>();
  for (const record of cache.entities) {
    if (!isLinkedAccountEntityType(record.entityType)) {
      continue;
    }
    const version = entityVersion(record);
    if (record.operation === "delete") {
      const identity =
        linkedAccountIdentityFromPayload(record.payload) ??
        linkedAccountIdentityFromEntityId(record.entityId);
      if (identity) {
        if (versionIsNewer(version, tombstoneVersions.get(identity.key))) {
          tombstones.add(identity.key);
          tombstoneVersions.set(identity.key, version);
        }
      } else if (versionIsNewer(version, legacyTombstoneVersions.get(record.entityId))) {
        legacyTombstoneIds.add(record.entityId);
        legacyTombstoneVersions.set(record.entityId, version);
      }
      continue;
    }
    if (!isRecord(record.payload)) {
      continue;
    }
    const identity = linkedAccountIdentityFromPayload(record.payload);
    if (identity && versionIsNewer(version, upserts.get(identity.key)?.version)) {
      upserts.set(identity.key, { identity, payload: record.payload, version });
    }
  }
  return {
    upserts,
    tombstones,
    tombstoneVersions,
    legacyTombstoneIds,
    legacyTombstoneVersions,
  };
}

function projectLinkedAccounts(
  record: WebSyncEntityRecord,
  projection: LinkedAccountProjection,
): unknown {
  const subscription = record.payload;
  if (!isRecord(subscription)) {
    return subscription;
  }
  const streamerId = payloadId(subscription);
  if (!streamerId) {
    return subscription;
  }
  const subscriptionVersion = entityVersion(record);
  const links = new Map<string, Record<string, unknown>>();
  const baseLinks = Array.isArray(subscription.linkedAccounts) ? subscription.linkedAccounts : [];
  for (const link of baseLinks) {
    if (!isRecord(link)) {
      continue;
    }
    const mid = idValue(link.mid);
    if (!mid) {
      continue;
    }
    const platform = platformValue(link.platform);
    const linkKey = `${platform}\u0000${mid}`;
    const baseVersion: EntityVersion = {
      revision: Math.max(subscriptionVersion.revision, nonNegativeInteger(link.revision)),
      updatedAt: subscriptionVersion.updatedAt,
      ...(subscriptionVersion.seq === undefined ? {} : { seq: subscriptionVersion.seq }),
    };
    const stableTombstone = projection.tombstoneVersions.get(`${streamerId}\u0000${linkKey}`);
    if (stableTombstone && !versionIsNewer(baseVersion, stableTombstone)) {
      continue;
    }
    const legacyId = idValue(link.id);
    const legacyTombstone = legacyId
      ? projection.legacyTombstoneVersions.get(legacyId)
      : undefined;
    if (legacyTombstone && !versionIsNewer(baseVersion, legacyTombstone)) {
      continue;
    }
    links.set(linkKey, normalizedLinkedAccount(link));
  }
  for (const { identity, payload, version } of projection.upserts.values()) {
    if (identity.streamerId !== streamerId) {
      continue;
    }
    const stableTombstone = projection.tombstoneVersions.get(identity.key);
    if (stableTombstone && !versionIsNewer(version, stableTombstone)) {
      continue;
    }
    const payloadIdValue = idValue(payload.id);
    const legacyTombstone = payloadIdValue
      ? projection.legacyTombstoneVersions.get(payloadIdValue)
      : undefined;
    if (legacyTombstone && !versionIsNewer(version, legacyTombstone)) {
      continue;
    }
    links.set(linkedAccountKey(identity.platform, identity.mid) ?? identity.mid, {
      ...(links.get(linkedAccountKey(identity.platform, identity.mid) ?? identity.mid) ?? {}),
      ...normalizedLinkedAccount(payload),
    });
  }
  return {
    ...normalizedSubscription(subscription),
    linkedAccounts: Array.from(links.values()).sort((left, right) =>
      stringValue(left.name).localeCompare(stringValue(right.name), "zh-CN")
        || stringValue(left.platform).localeCompare(stringValue(right.platform)),
    ),
  };
}

function makeEntityRecord(
  serverKey: string,
  entityType: string,
  entityId: string,
  payload: unknown,
  updatedAt: string,
  entityRevision = payloadRevision(payload),
  operation: "upsert" | "delete" = "upsert",
  seq?: number,
): WebSyncEntityRecord {
  return {
    cacheKey: webSyncEntityCacheKey(serverKey, entityType, entityId),
    serverKey,
    entityType,
    entityId,
    entityRevision,
    operation,
    ...(seq === undefined ? {} : { seq }),
    payload: operation === "delete" ? payload : normalizedEntityPayload(entityType, payload),
    updatedAt,
  };
}

function bootstrapRecords(
  serverKey: string,
  snapshot: SyncBootstrap,
  updatedAt = new Date().toISOString(),
): WebSyncEntityRecord[] {
  const records: WebSyncEntityRecord[] = [];
  const add = (entityType: string, entityId: string | null, payload: unknown) => {
    if (entityId) {
      records.push(makeEntityRecord(serverKey, entityType, entityId, payload, updatedAt));
    }
  };

  for (const item of snapshot.subscriptions) {
    add("subscription", payloadId(item), item);
  }
  for (const item of snapshot.schedules) {
    add("schedule", payloadId(item), item);
  }
  for (const item of snapshot.weeklyRules) {
    add("weekly_rule", payloadId(item), item);
  }
  for (const item of snapshot.weeklyExceptions) {
    add("weekly_exception", payloadId(item), item);
  }
  for (const item of snapshot.scheduleSources) {
    add("schedule_source", payloadId(item), item);
  }
  for (const item of snapshot.sourceSuppressions) {
    add("source_suppression", payloadId(item), item);
  }
  for (const item of snapshot.liveStatuses) {
    add("live_status", payloadId(item, "streamerId"), item);
  }
  for (const item of snapshot.liveSessions) {
    add("live_session", payloadId(item), item);
  }
  add(
    "server_settings",
    snapshot.serverSettings ? SERVER_SETTINGS_ENTITY_ID : null,
    snapshot.serverSettings,
  );
  return records;
}

export function applySyncChangesToEntities(
  serverKey: string,
  existing: WebSyncEntityRecord[],
  changes: SyncChange[],
): WebSyncEntityRecord[] {
  const byCacheKey = new Map<string, WebSyncEntityRecord>();
  for (const record of existing) {
    if (record.serverKey === serverKey) {
      byCacheKey.set(record.cacheKey, record);
    }
  }
  const updatedAt = new Date().toISOString();
  for (const change of changes) {
    const key = webSyncEntityCacheKey(serverKey, change.entityType, change.entityId);
    if (change.operation === "delete") {
      if (isLinkedAccountEntityType(change.entityType)) {
        byCacheKey.set(
          key,
          makeEntityRecord(
            serverKey,
            change.entityType,
            change.entityId,
            change.payload,
            updatedAt,
            change.entityRevision,
            "delete",
            change.seq,
          ),
        );
      } else {
        byCacheKey.delete(key);
      }
      continue;
    }
    if (change.payload === null) {
      continue;
    }
    byCacheKey.set(
      key,
      makeEntityRecord(
        serverKey,
        change.entityType,
        change.entityId,
        change.payload,
        updatedAt,
        change.entityRevision,
        "upsert",
        change.seq,
      ),
    );
  }
  return Array.from(byCacheKey.values());
}

function recordsForType(cache: Pick<WebSyncCache, "entities">, entityType: string): unknown[] {
  return cache.entities
    .filter(
      (record) =>
        record.entityType === entityType &&
        record.operation !== "delete" &&
        isRecord(record.payload),
    )
    .map((record) => record.payload);
}

export function syncBootstrapFromCache(cache: Pick<WebSyncCache, "meta" | "entities">): SyncBootstrap {
  const settingsRecord = cache.entities.find(
    (record) => record.entityType === "server_settings" && isRecord(record.payload),
  );
  const linkProjection = linkedAccountProjection(cache);
  return {
    schemaVersion: cache.meta.schemaVersion,
    feedId: stringValue(cache.meta.feedId).trim(),
    snapshotRevision: cache.meta.snapshotRevision,
    minRetainedSeq: cache.meta.minRetainedSeq,
    serverTime: cache.meta.updatedAt,
    subscriptions: cache.entities
      .filter(
        (record) =>
          record.entityType === "subscription" &&
          record.operation !== "delete" &&
          isRecord(record.payload),
      )
      .map((record) => projectLinkedAccounts(record, linkProjection) as Streamer),
    schedules: recordsForType(cache, "schedule") as Schedule[],
    weeklyRules: recordsForType(cache, "weekly_rule") as WeeklyScheduleRule[],
    weeklyExceptions: recordsForType(cache, "weekly_exception") as WeeklyScheduleException[],
    scheduleSources: recordsForType(cache, "schedule_source") as ScheduleSource[],
    sourceSuppressions: recordsForType(cache, "source_suppression") as SourceSuppression[],
    serverSettings: isRecord(settingsRecord?.payload) ? settingsRecord.payload : {},
    liveStatuses: recordsForType(cache, "live_status") as LiveStatus[],
    liveSessions: recordsForType(cache, "live_session") as LiveSession[],
  };
}

export function isWebSyncCacheUsable(cache: WebSyncCache | null): cache is WebSyncCache {
  if (!cache || !cache.meta.initialized) {
    return false;
  }
  const meta = cache.meta;
  return (
    meta.schemaVersion === WEB_SYNC_CACHE_SCHEMA_VERSION &&
    meta.cursorSeq >= Math.max(0, meta.minRetainedSeq - 1) &&
    meta.cursorSeq <= meta.latestSeq
  );
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("IndexedDB request failed"));
  });
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error ?? new Error("IndexedDB transaction failed"));
    transaction.onabort = () => reject(transaction.error ?? new Error("IndexedDB transaction aborted"));
  });
}

function openWebSyncDatabase(): Promise<IDBDatabase | null> {
  if (databasePromise) {
    return databasePromise;
  }
  let factory: IDBFactory | null = null;
  try {
    factory = typeof indexedDB === "undefined" ? null : indexedDB;
  } catch {
    factory = null;
  }
  if (!factory) {
    databasePromise = Promise.resolve(null);
    return databasePromise;
  }
  databasePromise = new Promise<IDBDatabase | null>((resolve) => {
    let settled = false;
    const finish = (database: IDBDatabase | null) => {
      if (!settled) {
        settled = true;
        resolve(database);
      }
    };
    let request: IDBOpenDBRequest;
    try {
      request = factory.open(WEB_SYNC_DB_NAME, WEB_SYNC_DB_VERSION);
    } catch {
      databasePromise = null;
      finish(null);
      return;
    }
    request.onupgradeneeded = () => {
      const database = request.result;
      if (!database.objectStoreNames.contains(META_STORE)) {
        database.createObjectStore(META_STORE, { keyPath: "serverKey" });
      }
      if (!database.objectStoreNames.contains(ENTITY_STORE)) {
        const entities = database.createObjectStore(ENTITY_STORE, { keyPath: "cacheKey" });
        entities.createIndex(SERVER_INDEX, "serverKey", { unique: false });
      }
    };
    request.onsuccess = () => {
      const database = request.result;
      if (settled) {
        database.close();
        databasePromise = null;
        return;
      }
      database.onversionchange = () => {
        database.close();
        databasePromise = null;
      };
      finish(database);
    };
    request.onerror = () => {
      databasePromise = null;
      finish(null);
    };
    request.onblocked = () => {
      databasePromise = null;
      finish(null);
    };
  });
  return databasePromise;
}

export async function isWebSyncStorageAvailable(): Promise<boolean> {
  return (await openWebSyncDatabase()) !== null;
}

function normalizedMeta(serverKey: string, input: WebSyncCacheMetaInput): WebSyncCacheMeta {
  const cursorSeq = nonNegativeInteger(input.cursorSeq);
  return {
    serverKey,
    schemaVersion: WEB_SYNC_CACHE_SCHEMA_VERSION,
    feedId: stringValue(input.feedId).trim(),
    cursorSeq,
    latestSeq: nonNegativeInteger(input.latestSeq),
    minRetainedSeq: nonNegativeInteger(input.minRetainedSeq),
    snapshotRevision: nonNegativeInteger(input.snapshotRevision),
    ackedSeq: Math.min(cursorSeq, nonNegativeInteger(input.ackedSeq)),
    lastSyncAt: input.lastSyncAt,
    initialized: true,
    updatedAt: new Date().toISOString(),
  };
}

function errorValue(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function metaFromRecord(serverKey: string, value: unknown): WebSyncCacheMeta | null {
  if (!isRecord(value)) {
    return null;
  }
  const cursorSeq = nonNegativeInteger(value.cursorSeq);
  return {
    serverKey,
    schemaVersion: nonNegativeInteger(value.schemaVersion),
    feedId: stringValue(value.feedId).trim(),
    cursorSeq,
    latestSeq: nonNegativeInteger(value.latestSeq),
    minRetainedSeq: nonNegativeInteger(value.minRetainedSeq),
    snapshotRevision: nonNegativeInteger(value.snapshotRevision),
    ackedSeq: Math.min(cursorSeq, nonNegativeInteger(value.ackedSeq)),
    lastSyncAt: typeof value.lastSyncAt === "string" ? value.lastSyncAt : null,
    initialized: value.initialized === true,
    updatedAt: stringValue(value.updatedAt, new Date(0).toISOString()),
  };
}

function entityFromRecord(serverKey: string, value: unknown): WebSyncEntityRecord | null {
  if (!isRecord(value)) {
    return null;
  }
  const entityType = stringValue(value.entityType);
  const entityId = stringValue(value.entityId);
  if (!entityType || !entityId || value.serverKey !== serverKey) {
    return null;
  }
  const seq =
    typeof value.seq === "number" && Number.isFinite(value.seq)
      ? nonNegativeInteger(value.seq)
      : undefined;
  return {
    cacheKey: webSyncEntityCacheKey(serverKey, entityType, entityId),
    serverKey,
    entityType,
    entityId,
    entityRevision: nonNegativeInteger(value.entityRevision),
    operation: value.operation === "delete" ? "delete" : "upsert",
    ...(seq === undefined ? {} : { seq }),
    payload: value.payload ?? null,
    updatedAt: stringValue(value.updatedAt, new Date(0).toISOString()),
  };
}

export async function loadWebSyncCache(baseUrl: string): Promise<WebSyncCacheLoadResult> {
  const serverKey = normalizeWebSyncServerKey(baseUrl);
  const database = await openWebSyncDatabase();
  if (!database || !serverKey) {
    if (serverKey) {
      loadedCacheFeedIds.delete(serverKey);
      loadedCacheServerKeys.delete(serverKey);
    }
    return { available: false, cache: null };
  }
  try {
    const transaction = database.transaction([META_STORE, ENTITY_STORE], "readonly");
    const done = transactionDone(transaction);
    void done.catch(() => undefined);
    const metaRequest = transaction.objectStore(META_STORE).get(serverKey);
    const entityStore = transaction.objectStore(ENTITY_STORE);
    const entityRequest = entityStore.index(SERVER_INDEX).getAll(serverKey);
    const [metaValue, entityValues] = await Promise.all([
      requestResult(metaRequest),
      requestResult(entityRequest),
    ]);
    await done;
    const meta = metaFromRecord(serverKey, metaValue);
    if (!meta) {
      loadedCacheFeedIds.delete(serverKey);
      loadedCacheServerKeys.delete(serverKey);
      return { available: true, cache: null };
    }
    const entities = (Array.isArray(entityValues) ? entityValues : [])
      .map((value) => entityFromRecord(serverKey, value))
      .filter((record): record is WebSyncEntityRecord => Boolean(record));
    const cache: WebSyncCache = { meta, entities, snapshot: {} as SyncBootstrap };
    cache.snapshot = syncBootstrapFromCache(cache);
    if (isWebSyncCacheUsable(cache)) {
      loadedCacheFeedIds.set(serverKey, stringValue(meta.feedId).trim());
      loadedCacheServerKeys.add(serverKey);
    } else {
      loadedCacheFeedIds.delete(serverKey);
      loadedCacheServerKeys.delete(serverKey);
    }
    return { available: true, cache };
  } catch {
    loadedCacheFeedIds.delete(serverKey);
    loadedCacheServerKeys.delete(serverKey);
    return { available: true, cache: null };
  }
}

export async function replaceWebSyncCacheWithResult(
  baseUrl: string,
  snapshot: SyncBootstrap,
  input: WebSyncCacheMetaInput,
): Promise<WebSyncCacheWriteResult> {
  if (snapshot.schemaVersion !== WEB_SYNC_CACHE_SCHEMA_VERSION) {
    return {
      status: "failed",
      error: new Error(
        `同步快照 schema 不兼容（服务器 ${snapshot.schemaVersion}，缓存 ${WEB_SYNC_CACHE_SCHEMA_VERSION}）`,
      ),
    };
  }
  const snapshotFeedId = stringValue(snapshot.feedId).trim();
  const inputFeedId = stringValue(input.feedId).trim();
  if (
    input.cursorSeq !== snapshot.snapshotRevision ||
    input.snapshotRevision !== snapshot.snapshotRevision ||
    (input.feedId !== undefined && inputFeedId !== snapshotFeedId)
  ) {
    return {
      status: "failed",
      error: new Error("同步快照与缓存元数据身份不一致"),
    };
  }
  const serverKey = normalizeWebSyncServerKey(baseUrl);
  const database = await openWebSyncDatabase();
  if (!database || !serverKey) {
    return { status: "unavailable", error: null };
  }
  let transaction: IDBTransaction | null = null;
  let transactionStarted = false;
  try {
    transaction = database.transaction([META_STORE, ENTITY_STORE], "readwrite");
    transactionStarted = true;
    const done = transactionDone(transaction);
    void done.catch(() => undefined);
    const entityStore = transaction.objectStore(ENTITY_STORE);
    const metaStore = transaction.objectStore(META_STORE);
    const records = bootstrapRecords(serverKey, snapshot);
    const existingKeys = entityStore.index(SERVER_INDEX).getAllKeys(serverKey);
    existingKeys.onsuccess = () => {
      try {
        for (const key of existingKeys.result) {
          entityStore.delete(key);
        }
        for (const record of records) {
          entityStore.put(record);
        }
        metaStore.put(normalizedMeta(serverKey, input));
      } catch {
        try {
          transaction?.abort();
        } catch {
          // The transaction may already have aborted.
        }
      }
    };
    await done;
    loadedCacheFeedIds.set(serverKey, stringValue(input.feedId).trim());
    loadedCacheServerKeys.add(serverKey);
    return { status: "persisted", error: null };
  } catch (error) {
    try {
      transaction?.abort();
    } catch {
      // The transaction may already have aborted or completed.
    }
    if (transactionStarted || database) {
      databasePromise = null;
    }
    return {
      status: transactionStarted || database ? "failed" : "unavailable",
      error: errorValue(error),
    };
  }
}

export async function replaceWebSyncCache(
  baseUrl: string,
  snapshot: SyncBootstrap,
  input: WebSyncCacheMetaInput,
): Promise<boolean> {
  return (await replaceWebSyncCacheWithResult(baseUrl, snapshot, input)).status === "persisted";
}

export async function persistWebSyncBatchWithResult(
  baseUrl: string,
  changes: SyncChange[],
  input: WebSyncCacheMetaInput,
): Promise<WebSyncCacheWriteResult> {
  const serverKey = normalizeWebSyncServerKey(baseUrl);
  const database = await openWebSyncDatabase();
  if (!database || !serverKey) {
    return { status: "unavailable", error: null };
  }
  let transaction: IDBTransaction | null = null;
  let transactionStarted = false;
  try {
    transaction = database.transaction([META_STORE, ENTITY_STORE], "readwrite");
    transactionStarted = true;
    const done = transactionDone(transaction);
    void done.catch(() => undefined);
    const entityStore = transaction.objectStore(ENTITY_STORE);
    const updatedAt = new Date().toISOString();
    for (const change of changes) {
      const key = webSyncEntityCacheKey(serverKey, change.entityType, change.entityId);
      if (change.operation === "delete") {
        if (isLinkedAccountEntityType(change.entityType)) {
          entityStore.put(
            makeEntityRecord(
              serverKey,
              change.entityType,
              change.entityId,
              change.payload,
              updatedAt,
              change.entityRevision,
              "delete",
              change.seq,
            ),
          );
        } else {
          entityStore.delete(key);
        }
        continue;
      }
      if (change.payload === null) {
        throw new Error(`同步实体 ${change.entityType}:${change.entityId} 缺少 payload`);
      }
      entityStore.put(
        makeEntityRecord(
          serverKey,
          change.entityType,
          change.entityId,
          change.payload,
          updatedAt,
          change.entityRevision,
          "upsert",
          change.seq,
        ),
      );
    }
    transaction.objectStore(META_STORE).put(normalizedMeta(serverKey, input));
    await done;
    loadedCacheFeedIds.set(serverKey, stringValue(input.feedId).trim());
    loadedCacheServerKeys.add(serverKey);
    return { status: "persisted", error: null };
  } catch (error) {
    try {
      transaction?.abort();
    } catch {
      // The transaction may already have aborted or completed.
    }
    if (transactionStarted || database) {
      databasePromise = null;
    }
    return {
      status: transactionStarted || database ? "failed" : "unavailable",
      error: errorValue(error),
    };
  }
}

export async function persistWebSyncBatch(
  baseUrl: string,
  changes: SyncChange[],
  input: WebSyncCacheMetaInput,
): Promise<boolean> {
  return (await persistWebSyncBatchWithResult(baseUrl, changes, input)).status === "persisted";
}
