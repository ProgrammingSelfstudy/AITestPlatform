/**
 * 浏览器里的草稿暂存（IndexedDB）：文字和图片都存，刷新页面、前端热更新、误关页面之后还在。
 * - 只存在这个浏览器里，不上传；超过 30 天的草稿读取时丢掉。
 * - 存取全部包在 try/catch 里：IndexedDB 不可用（隐私模式、测试环境）时退化成内存，页面照常能用，只是刷新后不会保留。
 * - 不要往里放密码、API Key（调用方负责）。
 */

const DB_NAME = "platform-drafts";
const STORE = "drafts";
const TTL_MS = 30 * 24 * 60 * 60 * 1000;

interface Saved<T> {
  value: T;
  savedAt: number;
}

const memory = new Map<string, Saved<unknown>>();

function openDb(): Promise<IDBDatabase | null> {
  return new Promise((resolve) => {
    try {
      if (typeof indexedDB === "undefined") return resolve(null);
      const request = indexedDB.open(DB_NAME, 1);
      request.onupgradeneeded = () => request.result.createObjectStore(STORE);
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => resolve(null);
      request.onblocked = () => resolve(null);
    } catch {
      resolve(null);
    }
  });
}

function run<T>(db: IDBDatabase, mode: IDBTransactionMode, work: (store: IDBObjectStore) => IDBRequest<T>): Promise<T | undefined> {
  return new Promise((resolve) => {
    try {
      const tx = db.transaction(STORE, mode);
      const request = work(tx.objectStore(STORE));
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => resolve(undefined);
      tx.onabort = () => resolve(undefined);
    } catch {
      resolve(undefined);
    }
  });
}

/** 保存草稿；同一个 key 会覆盖。失败（空间不够等）返回 false，不抛错。 */
export async function saveDraft<T>(key: string, value: T): Promise<boolean> {
  const saved: Saved<T> = { value, savedAt: Date.now() };
  memory.set(key, saved);
  const db = await openDb();
  if (!db) return true;
  try {
    const done = await new Promise<boolean>((resolve) => {
      try {
        const tx = db.transaction(STORE, "readwrite");
        tx.objectStore(STORE).put(saved, key);
        tx.oncomplete = () => resolve(true);
        tx.onerror = () => resolve(false);
        tx.onabort = () => resolve(false);
      } catch {
        resolve(false);
      }
    });
    return done;
  } finally {
    db.close();
  }
}

/** 读草稿（含保存时间）。没有、过期、读不出来都返回 null。 */
export async function loadDraft<T>(key: string): Promise<{ value: T; savedAt: number } | null> {
  const db = await openDb();
  let saved: Saved<T> | undefined;
  if (db) {
    saved = (await run<Saved<T>>(db, "readonly", (store) => store.get(key))) ?? undefined;
    db.close();
  }
  saved ??= memory.get(key) as Saved<T> | undefined;
  if (!saved || typeof saved.savedAt !== "number") return null;
  if (Date.now() - saved.savedAt > TTL_MS) {
    void clearDraft(key);
    return null;
  }
  return saved;
}

export async function clearDraft(key: string): Promise<void> {
  memory.delete(key);
  const db = await openDb();
  if (!db) return;
  await run(db, "readwrite", (store) => store.delete(key));
  db.close();
}
