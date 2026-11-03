import type {
  LiveSession,
  LiveStatus,
  Schedule,
  ScheduleSource,
  SourceSuppression,
  Streamer,
  SyncBootstrap,
  SyncChange,
  WeeklyScheduleException,
  WeeklyScheduleRule,
} from "../types";

export const WEB_SYNC_CACHE_SCHEMA_VERSION = 2;
const WEB_SYNC_DB_NAME = "oshical.web-sync.v2";
const WEB_SYNC_DB_VERSION = 1;
const META_STORE = "sync_meta";
const ENTITY_STORE = "sync_entity_cache";
const SERVER_INDEX = "serverKey";
const SERVER_SETTINGS_ENTITY_ID = "server_settings";

export interface WebSyncCacheMeta {
  serverKey: string;
  schemaVersion: number;
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
  cursorSeq: number;
  latestSeq: number;
  minRetainedSeq: number;
  snapshotRevision: number;
  ackedSeq: number;
  lastSyncAt: string | null;
}

let databasePromise: Promise<IDBDatabase | null> | null = null;

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

function makeEntityRecord(
  serverKey: string,
  entityType: string,
  entityId: string,
  payload: unknown,
  updatedAt: string,
  entityRevision = payloadRevision(payload),
): WebSyncEntityRecord {
  return {
    cacheKey: webSyncEntityCacheKey(serverKey, entityType, entityId),
    serverKey,
    entityType,
    entityId,
    entityRevision,
    payload,
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
      byCacheKey.delete(key);
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
      ),
    );
  }
  return Array.from(byCacheKey.values());
}

function recordsForType(cache: Pick<WebSyncCache, "entities">, entityType: string): unknown[] {
  return cache.entities
    .filter((record) => record.entityType === entityType && isRecord(record.payload))
    .map((record) => record.payload);
}

export function syncBootstrapFromCache(cache: Pick<WebSyncCache, "meta" | "entities">): SyncBootstrap {
  const settingsRecord = cache.entities.find(
    (record) => record.entityType === "server_settings" && isRecord(record.payload),
  );
  return {
    schemaVersion: cache.meta.schemaVersion,
    snapshotRevision: cache.meta.snapshotRevision,
    minRetainedSeq: cache.meta.minRetainedSeq,
    serverTime: cache.meta.updatedAt,
    subscriptions: recordsForType(cache, "subscription") as Streamer[],
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
      database.onversionchange = () => database.close();
      finish(database);
    };
    request.onerror = () => finish(null);
    request.onblocked = () => finish(null);
  });
  return databasePromise;
}

export async function isWebSyncStorageAvailable(): Promise<boolean> {
  return (await openWebSyncDatabase()) !== null;
}

function normalizedMeta(serverKey: string, input: WebSyncCacheMetaInput): WebSyncCacheMeta {
  return {
    serverKey,
    schemaVersion: WEB_SYNC_CACHE_SCHEMA_VERSION,
    cursorSeq: nonNegativeInteger(input.cursorSeq),
    latestSeq: Math.max(
      nonNegativeInteger(input.latestSeq),
      nonNegativeInteger(input.cursorSeq),
    ),
    minRetainedSeq: nonNegativeInteger(input.minRetainedSeq),
    snapshotRevision: nonNegativeInteger(input.snapshotRevision),
    ackedSeq: nonNegativeInteger(input.ackedSeq),
    lastSyncAt: input.lastSyncAt,
    initialized: true,
    updatedAt: new Date().toISOString(),
  };
}

function metaFromRecord(serverKey: string, value: unknown): WebSyncCacheMeta | null {
  if (!isRecord(value)) {
    return null;
  }
  return {
    serverKey,
    schemaVersion: nonNegativeInteger(value.schemaVersion),
    cursorSeq: nonNegativeInteger(value.cursorSeq),
    latestSeq: nonNegativeInteger(value.latestSeq),
    minRetainedSeq: nonNegativeInteger(value.minRetainedSeq),
    snapshotRevision: nonNegativeInteger(value.snapshotRevision),
    ackedSeq: nonNegativeInteger(value.ackedSeq),
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
  return {
    cacheKey: webSyncEntityCacheKey(serverKey, entityType, entityId),
    serverKey,
    entityType,
    entityId,
    entityRevision: nonNegativeInteger(value.entityRevision),
    payload: value.payload ?? null,
    updatedAt: stringValue(value.updatedAt, new Date(0).toISOString()),
  };
}

export async function loadWebSyncCache(baseUrl: string): Promise<WebSyncCacheLoadResult> {
  const serverKey = normalizeWebSyncServerKey(baseUrl);
  const database = await openWebSyncDatabase();
  if (!database || !serverKey) {
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
      return { available: true, cache: null };
    }
    const entities = (Array.isArray(entityValues) ? entityValues : [])
      .map((value) => entityFromRecord(serverKey, value))
      .filter((record): record is WebSyncEntityRecord => Boolean(record));
    const cache: WebSyncCache = { meta, entities, snapshot: {} as SyncBootstrap };
    cache.snapshot = syncBootstrapFromCache(cache);
    return { available: true, cache };
  } catch {
    return { available: true, cache: null };
  }
}

export async function replaceWebSyncCache(
  baseUrl: string,
  snapshot: SyncBootstrap,
  input: WebSyncCacheMetaInput,
): Promise<boolean> {
  const serverKey = normalizeWebSyncServerKey(baseUrl);
  const database = await openWebSyncDatabase();
  if (!database || !serverKey) {
    return false;
  }
  let transaction: IDBTransaction | null = null;
  try {
    transaction = database.transaction([META_STORE, ENTITY_STORE], "readwrite");
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
    return true;
  } catch {
    try {
      transaction?.abort();
    } catch {
      // The transaction may already have aborted or completed.
    }
    return false;
  }
}

export async function persistWebSyncBatch(
  baseUrl: string,
  changes: SyncChange[],
  input: WebSyncCacheMetaInput,
): Promise<boolean> {
  const serverKey = normalizeWebSyncServerKey(baseUrl);
  const database = await openWebSyncDatabase();
  if (!database || !serverKey) {
    return false;
  }
  if (!changes.length) {
    return true;
  }
  let transaction: IDBTransaction | null = null;
  try {
    transaction = database.transaction([META_STORE, ENTITY_STORE], "readwrite");
    const done = transactionDone(transaction);
    void done.catch(() => undefined);
    const entityStore = transaction.objectStore(ENTITY_STORE);
    const updatedAt = new Date().toISOString();
    for (const change of changes) {
      const key = webSyncEntityCacheKey(serverKey, change.entityType, change.entityId);
      if (change.operation === "delete") {
        entityStore.delete(key);
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
        ),
      );
    }
    transaction.objectStore(META_STORE).put(normalizedMeta(serverKey, input));
    await done;
    return true;
  } catch {
    try {
      transaction?.abort();
    } catch {
      // The transaction may already have aborted or completed.
    }
    return false;
  }
}
