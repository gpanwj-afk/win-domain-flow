const DEFAULT_RECEIVER_PORT = 38765;
const LOCAL_RECEIVER_HOST = "127.0.0.1";
const PROTOCOL_VERSION = "1.3";
const PARTIAL_REPORT_BYTES = 1024 * 1024;
const PARTIAL_REPORT_MS = 5000;
const MAX_QUEUE_LENGTH = 1000;
// Chrome 113 and earlier expose roughly 5 MiB for storage.local. Reserve
// headroom for settings and browser bookkeeping instead of relying only on a
// count limit, because a small number of long URLs can otherwise exhaust it.
const MAX_QUEUE_BYTES = 4 * 1024 * 1024;
const RETRY_BASE_MS = 500;
const RETRY_MAX_MS = 30000;
const RETRY_ALARM = "domainflow-retry";
const QUEUE_STORAGE_KEY = "pendingEventQueue";

// Request state is kept in memory while a tab is attached. Delivery state is
// persisted separately so a suspended Manifest V3 worker does not lose events.
const pending = new Map();
const attachedTabs = new Set();
const attachingTabs = new Set();
const tabStates = new Map();
const requestSequences = new Map();
let diagnosticsEnabled = false;
let receiverPort = DEFAULT_RECEIVER_PORT;
let eventQueue = [];
let queueRunning = false;
let lastAttachError = null;
let lastReceiverError = null;
let droppedEventCount = 0;

function receiverUrl(path) {
  return `http://${LOCAL_RECEIVER_HOST}:${receiverPort}${path}`;
}

function safeUrl(value) {
  try {
    return new URL(value || "");
  } catch {
    return null;
  }
}

function hostOf(value) {
  const parsed = safeUrl(value);
  return parsed ? parsed.hostname.toLowerCase() : "";
}

function isInspectableUrl(value) {
  const parsed = safeUrl(value);
  return Boolean(parsed && (parsed.protocol === "http:" || parsed.protocol === "https:"));
}

function shouldIgnore(url) {
  if (!url) return true;
  const parsed = safeUrl(url);
  if (parsed && parsed.hostname === LOCAL_RECEIVER_HOST && Number(parsed.port) === receiverPort) {
    return true;
  }
  return url.startsWith("chrome-extension://") || url.startsWith("edge-extension://");
}

function requestKey(tabId, requestId) {
  return `${tabId}:${requestId}`;
}

function nextEventId(tabId, requestId, timestampMs) {
  const key = requestKey(tabId, requestId);
  const sequence = (requestSequences.get(key) || 0) + 1;
  requestSequences.set(key, sequence);
  return `cdp:${tabId}:${requestId}:${sequence}:${timestampMs}`;
}

function positiveInteger(value) {
  if (!Number.isFinite(value) || value < 0) return null;
  const rounded = Math.round(value);
  return Number.isSafeInteger(rounded) ? rounded : null;
}

function validPort(value) {
  const port = Number(value);
  return Number.isInteger(port) && port >= 1024 && port <= 65535 ? port : DEFAULT_RECEIVER_PORT;
}

function headerValue(headers, name) {
  if (!headers || typeof headers !== "object") return null;
  const wanted = name.toLowerCase();
  for (const [key, value] of Object.entries(headers)) {
    if (key.toLowerCase() === wanted) return String(value);
  }
  return null;
}

function parseDeclaredBytes(headers) {
  const contentLength = headerValue(headers, "content-length");
  if (contentLength && /^\d+$/.test(contentLength.trim())) {
    return positiveInteger(Number(contentLength.trim()));
  }

  const contentRange = headerValue(headers, "content-range");
  if (contentRange) {
    const match = /bytes\s+(\d+)-(\d+)\/(\d+|\*)/i.exec(contentRange);
    if (match) {
      const start = Number(match[1]);
      const end = Number(match[2]);
      if (Number.isSafeInteger(start) && Number.isSafeInteger(end) && end >= start) {
        return end - start + 1;
      }
    }
  }
  return null;
}

function cleanMime(value) {
  return value ? String(value).split(";", 1)[0].trim().toLowerCase() : null;
}

function formatInitiator(initiator) {
  if (!initiator || typeof initiator !== "object") return null;
  if (typeof initiator.url === "string" && initiator.url) return initiator.url;
  if (initiator.stack && Array.isArray(initiator.stack.callFrames) && initiator.stack.callFrames.length > 0) {
    const frame = initiator.stack.callFrames.find((item) => item && item.url) || initiator.stack.callFrames[0];
    if (frame && frame.url) return frame.url;
  }
  return initiator.type ? String(initiator.type) : null;
}

function eventTimestampMs(params) {
  if (Number.isFinite(params && params.wallTime)) return Math.round(params.wallTime * 1000);
  return Date.now();
}

function baseRequest(tabId, params) {
  const request = params.request || {};
  const timestampMs = eventTimestampMs(params);
  return {
    eventId: nextEventId(tabId, params.requestId, timestampMs),
    timestampMs,
    host: hostOf(request.url),
    url: request.url || "",
    pageUrl: params.documentURL || null,
    initiator: formatInitiator(params.initiator),
    method: request.method || null,
    resourceType: String(params.type || "other").toLowerCase(),
    statusCode: null,
    mime: null,
    declaredBytes: null,
    // null means "not measured yet". A real zero is only written after CDP
    // explicitly reports zero encoded bytes for this request.
    transferredBytes: null,
    protocol: null,
    fromCache: false,
    contentDisposition: null,
    errorText: null,
    lastReportedBytes: 0,
    lastReportedAt: 0,
    reportChain: Promise.resolve()
  };
}

function applyResponseMetadata(item, response, resourceType = null) {
  if (!item || !response) return;
  item.statusCode = positiveInteger(response.status);
  item.mime = cleanMime(response.mimeType || headerValue(response.headers, "content-type"));
  item.declaredBytes = parseDeclaredBytes(response.headers);
  item.contentDisposition = headerValue(response.headers, "content-disposition");
  item.protocol = response.protocol || item.protocol || null;
  item.fromCache = Boolean(
    item.fromCache || response.fromDiskCache || response.fromServiceWorker || response.fromPrefetchCache
  );
  if (resourceType) item.resourceType = String(resourceType).toLowerCase();
  const encoded = positiveInteger(response.encodedDataLength);
  if (encoded !== null) {
    item.transferredBytes = Math.max(item.transferredBytes ?? 0, encoded);
  }
}

function tabState(tabId) {
  if (!tabStates.has(tabId)) {
    tabStates.set(tabId, {
      tabId,
      attached: false,
      title: null,
      url: null,
      lastAttachError: null,
      lastEventAt: null
    });
  }
  return tabStates.get(tabId);
}

function updateTabState(tabId, patch) {
  const current = tabState(tabId);
  Object.assign(current, patch);
}

function mergePayload(previous, next) {
  const merged = { ...previous };
  for (const [key, value] of Object.entries(next)) {
    if (value !== null && value !== undefined) merged[key] = value;
  }

  const oldBytes = positiveInteger(previous.transferredBytes);
  const newBytes = positiveInteger(next.transferredBytes);
  if (oldBytes !== null || newBytes !== null) {
    merged.transferredBytes = Math.max(oldBytes ?? 0, newBytes ?? 0);
  }
  merged.fromCache = Boolean(previous.fromCache || next.fromCache);
  return merged;
}

function queueStorageBytes() {
  try {
    const serialized = JSON.stringify({
      [QUEUE_STORAGE_KEY]: eventQueue,
      droppedEventCount
    });
    return new TextEncoder().encode(serialized).byteLength;
  } catch {
    return Number.MAX_SAFE_INTEGER;
  }
}

function trimQueueToBounds() {
  let dropped = 0;
  while (
    eventQueue.length > 0 &&
    (eventQueue.length > MAX_QUEUE_LENGTH || queueStorageBytes() > MAX_QUEUE_BYTES)
  ) {
    eventQueue.shift();
    dropped += 1;
  }
  if (dropped > 0) {
    droppedEventCount += dropped;
    lastReceiverError = `发送队列达到容量上限，已丢弃最旧事件（本次 ${dropped} 条，累计 ${droppedEventCount} 条）`;
  }
  return dropped;
}

async function persistQueue() {
  const dropped = trimQueueToBounds();
  try {
    await chrome.storage.local.set({
      [QUEUE_STORAGE_KEY]: eventQueue,
      droppedEventCount
    });
    return { ok: true, dropped };
  } catch (error) {
    lastReceiverError = `发送队列持久化失败：${error && error.message ? error.message : error}`;
    return { ok: false, dropped };
  }
}

async function scheduleRetry(whenMs) {
  try {
    await chrome.alarms.create(RETRY_ALARM, { when: Math.max(Date.now() + 100, whenMs) });
  } catch {
    // An in-memory retry is still attempted below if alarms are temporarily unavailable.
  }
}

async function deliverEvent(payload) {
  const response = await fetch(receiverUrl("/events"), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
    cache: "no-store"
  });

  let body = null;
  try {
    body = await response.json();
  } catch {
    throw new Error(`local receiver returned non-JSON response (${response.status})`);
  }
  if (!response.ok || !body || body.ok !== true || body.accepted !== true) {
    const detail = body && body.error ? `: ${body.error}` : "";
    throw new Error(`local receiver rejected event (${response.status})${detail}`);
  }
}

async function processQueue() {
  if (queueRunning) return;
  queueRunning = true;
  try {
    while (eventQueue.length > 0) {
      const entry = eventQueue[0];
      const now = Date.now();
      if (entry.nextAttemptAt && entry.nextAttemptAt > now) {
        await scheduleRetry(entry.nextAttemptAt);
        break;
      }

      const revision = Number(entry.revision) || 0;
      const payload = entry.payload;
      try {
        await deliverEvent(payload);
        if (eventQueue[0] === entry && (Number(entry.revision) || 0) === revision) {
          eventQueue.shift();
        } else {
          entry.attempts = 0;
          entry.nextAttemptAt = 0;
        }
        const persisted = await persistQueue();
        if (!persisted.ok) {
          await scheduleRetry(Date.now() + RETRY_BASE_MS);
          break;
        }
        if (persisted.dropped === 0) lastReceiverError = null;
      } catch (error) {
        entry.attempts = Math.max(0, Number(entry.attempts) || 0) + 1;
        const backoff = Math.min(RETRY_MAX_MS, RETRY_BASE_MS * (2 ** Math.min(entry.attempts - 1, 8)));
        entry.nextAttemptAt = Date.now() + backoff;
        lastReceiverError = String(error && error.message ? error.message : error);
        await persistQueue();
        await scheduleRetry(entry.nextAttemptAt);
        break;
      }
    }
    if (eventQueue.length === 0) {
      try {
        await chrome.alarms.clear(RETRY_ALARM);
      } catch {
        // Clearing a missing alarm is non-critical.
      }
    }
  } finally {
    queueRunning = false;
  }
}

function sameQueuedEvent(entry, payload) {
  return Boolean(
    entry &&
    entry.payload &&
    entry.payload.kind === payload.kind &&
    entry.payload.eventId === payload.eventId
  );
}

async function enqueueEvent(payload) {
  if (!payload || !payload.eventId) return false;
  const existingIndex = eventQueue.findIndex((entry) => sameQueuedEvent(entry, payload));
  if (existingIndex >= 0) {
    eventQueue[existingIndex].payload = mergePayload(eventQueue[existingIndex].payload, payload);
    eventQueue[existingIndex].revision = (Number(eventQueue[existingIndex].revision) || 0) + 1;
    eventQueue[existingIndex].attempts = 0;
    eventQueue[existingIndex].nextAttemptAt = 0;
  } else {
    eventQueue.push({ payload, attempts: 0, nextAttemptAt: 0, revision: 0 });
  }
  const persisted = await persistQueue();
  const retained = eventQueue.some((entry) => sameQueuedEvent(entry, payload));
  if (!persisted.ok) await scheduleRetry(Date.now() + RETRY_BASE_MS);
  processQueue();
  return retained && persisted.ok;
}

function requestPayload(item, errorText = null) {
  item.errorText = errorText || item.errorText;
  return {
    kind: "request",
    eventId: item.eventId,
    timestampMs: item.timestampMs,
    host: item.host,
    url: item.url,
    pageUrl: item.pageUrl,
    initiator: item.initiator,
    method: item.method,
    resourceType: item.resourceType,
    statusCode: item.statusCode,
    mime: item.mime,
    declaredBytes: item.declaredBytes,
    transferredBytes: positiveInteger(item.transferredBytes),
    protocol: item.protocol,
    fromCache: item.fromCache,
    contentDisposition: item.contentDisposition,
    errorText: item.errorText
  };
}

async function reportRequest(item, errorText = null) {
  if (!item || shouldIgnore(item.url) || !item.host) return false;
  const payload = requestPayload(item, errorText);
  // Recover from any previous asynchronous persistence error before appending
  // the next report. One transient storage failure must not poison this
  // request's report chain for the rest of its lifetime.
  item.reportChain = (item.reportChain || Promise.resolve())
    .catch(() => false)
    .then(() => enqueueEvent(payload))
    .catch((error) => {
      lastReceiverError = `发送队列处理失败：${error && error.message ? error.message : error}`;
      return false;
    });
  const queued = await item.reportChain;
  if (queued) {
    item.lastReportedBytes = item.transferredBytes ?? item.lastReportedBytes;
    item.lastReportedAt = Date.now();
  }
  return queued;
}

async function maybeReportPartial(item) {
  if (item.transferredBytes === null) return;
  const now = Date.now();
  const byteDelta = item.transferredBytes - (item.lastReportedBytes || 0);
  const timeDelta = now - item.lastReportedAt;
  if (byteDelta >= PARTIAL_REPORT_BYTES || (item.transferredBytes > 0 && timeDelta >= PARTIAL_REPORT_MS)) {
    await reportRequest(item);
  }
}

async function attachTab(tab) {
  if (
    !diagnosticsEnabled ||
    !tab ||
    !Number.isInteger(tab.id) ||
    !isInspectableUrl(tab.url) ||
    attachedTabs.has(tab.id) ||
    attachingTabs.has(tab.id)
  ) {
    return;
  }
  const target = { tabId: tab.id };
  let debuggerAttached = false;
  attachingTabs.add(tab.id);
  updateTabState(tab.id, { title: tab.title || null, url: tab.url || null });
  try {
    await chrome.debugger.attach(target, PROTOCOL_VERSION);
    debuggerAttached = true;
    await chrome.debugger.sendCommand(target, "Network.enable", {
      maxTotalBufferSize: 0,
      maxResourceBufferSize: 0,
      maxPostDataSize: 0
    });
    if (!diagnosticsEnabled) {
      await chrome.debugger.detach(target);
      debuggerAttached = false;
      updateTabState(tab.id, { attached: false, lastAttachError: null });
      return;
    }
    attachedTabs.add(tab.id);
    updateTabState(tab.id, { attached: true, lastAttachError: null });
    lastAttachError = null;
  } catch (error) {
    attachedTabs.delete(tab.id);
    if (debuggerAttached) {
      try {
        await chrome.debugger.detach(target);
      } catch {
        // The browser may already have detached the target.
      }
    }
    const message = `${tab.title || tab.url || `tab ${tab.id}`}: ${error && error.message ? error.message : error}`;
    updateTabState(tab.id, { attached: false, lastAttachError: message });
    lastAttachError = message;
  } finally {
    attachingTabs.delete(tab.id);
    await updateBadge();
  }
}

async function attachAllTabs() {
  if (!diagnosticsEnabled) return;
  const tabs = await chrome.tabs.query({});
  await Promise.all(tabs.map((tab) => attachTab(tab)));
}

async function detachAllTabs() {
  const tabIds = Array.from(attachedTabs);
  attachedTabs.clear();
  pending.clear();
  requestSequences.clear();
  await Promise.all(tabIds.map(async (tabId) => {
    try {
      await chrome.debugger.detach({ tabId });
    } catch {
      // The tab may already be closed or detached by DevTools.
    }
    updateTabState(tabId, { attached: false });
  }));
  await updateBadge();
}

async function setDiagnosticsEnabled(value) {
  diagnosticsEnabled = Boolean(value);
  await chrome.storage.local.set({ diagnosticsEnabled });
  if (diagnosticsEnabled) {
    await attachAllTabs();
  } else {
    await detachAllTabs();
  }
  await updateBadge();
}

async function updateBadge() {
  try {
    await chrome.action.setBadgeText({ text: diagnosticsEnabled ? String(attachedTabs.size) : "" });
    if (diagnosticsEnabled) {
      const healthy = attachedTabs.size > 0 && !lastReceiverError;
      await chrome.action.setBadgeBackgroundColor({ color: healthy ? "#047857" : "#b45309" });
    }
  } catch {
    // Badge support is non-critical.
  }
}

chrome.debugger.onEvent.addListener((source, method, params = {}) => {
  const tabId = source.tabId;
  if (!diagnosticsEnabled || !Number.isInteger(tabId)) return;
  updateTabState(tabId, { lastEventAt: Date.now() });
  const key = params.requestId ? requestKey(tabId, params.requestId) : null;

  if (method === "Network.requestWillBeSent") {
    if (!key || shouldIgnore(params.request && params.request.url)) return;

    // CDP reuses requestId across redirects. Finalize the previous hop before
    // creating a new event ID for the redirected URL so neither URL is lost.
    const previous = pending.get(key);
    if (params.redirectResponse && previous) {
      applyResponseMetadata(previous, params.redirectResponse, previous.resourceType);
      reportRequest(previous);
      pending.delete(key);
    } else if (previous) {
      reportRequest(previous, "request id reused before completion");
      pending.delete(key);
    }

    pending.set(key, baseRequest(tabId, params));
    return;
  }

  if (!key) return;
  const item = pending.get(key);
  if (!item) return;

  if (method === "Network.responseReceived") {
    applyResponseMetadata(item, params.response || {}, params.type);
    return;
  }

  if (method === "Network.requestServedFromCache") {
    item.fromCache = true;
    return;
  }

  if (method === "Network.dataReceived") {
    const increment = positiveInteger(params.encodedDataLength) ?? positiveInteger(params.dataLength);
    if (increment !== null) {
      item.transferredBytes = (item.transferredBytes ?? 0) + increment;
      maybeReportPartial(item);
    }
    return;
  }

  if (method === "Network.loadingFinished") {
    const total = positiveInteger(params.encodedDataLength);
    if (total !== null) item.transferredBytes = Math.max(item.transferredBytes ?? 0, total);
    pending.delete(key);
    requestSequences.delete(key);
    reportRequest(item);
    return;
  }

  if (method === "Network.loadingFailed") {
    pending.delete(key);
    requestSequences.delete(key);
    reportRequest(item, params.errorText || "request failed");
  }
});

chrome.debugger.onDetach.addListener((source, reason) => {
  if (!Number.isInteger(source.tabId)) return;
  attachedTabs.delete(source.tabId);
  updateTabState(source.tabId, { attached: false });
  for (const key of pending.keys()) {
    if (key.startsWith(`${source.tabId}:`)) {
      const item = pending.get(key);
      if (item) reportRequest(item, `debugger detached: ${reason || "unknown"}`);
      pending.delete(key);
      requestSequences.delete(key);
    }
  }
  if (diagnosticsEnabled && reason !== "canceled_by_user") {
    setTimeout(async () => {
      try {
        const tab = await chrome.tabs.get(source.tabId);
        await attachTab(tab);
      } catch {
        // Tab may be gone.
      }
    }, 1000);
  }
  updateBadge();
});

chrome.tabs.onCreated.addListener((tab) => {
  attachTab(tab);
});

chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (!diagnosticsEnabled) return;
  updateTabState(tabId, { title: tab.title || null, url: tab.url || null });
  if (changeInfo.url || changeInfo.status === "loading" || changeInfo.status === "complete") {
    attachTab({ ...tab, id: tabId });
  }
});

chrome.tabs.onRemoved.addListener((tabId) => {
  attachedTabs.delete(tabId);
  tabStates.delete(tabId);
  for (const key of pending.keys()) {
    if (key.startsWith(`${tabId}:`)) {
      pending.delete(key);
      requestSequences.delete(key);
    }
  }
  updateBadge();
});

function toMillis(value) {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

async function sendDownload(item) {
  if (!diagnosticsEnabled || !item || shouldIgnore(item.finalUrl || item.url)) return;
  const finalUrl = item.finalUrl || item.url;
  await enqueueEvent({
    kind: "download",
    eventId: `download:${item.id}`,
    timestampMs: toMillis(item.endTime) || Date.now(),
    startedAtMs: toMillis(item.startTime),
    endedAtMs: toMillis(item.endTime),
    host: hostOf(finalUrl),
    url: item.url,
    finalUrl,
    filename: item.filename || null,
    mime: item.mime || null,
    totalBytes: Number.isSafeInteger(item.totalBytes) && item.totalBytes >= 0 ? item.totalBytes : null,
    state: item.state || null,
    danger: item.danger || null,
    existsLocal: typeof item.exists === "boolean" ? item.exists : null
  });
}

chrome.downloads.onCreated.addListener((item) => {
  sendDownload(item);
});

chrome.downloads.onChanged.addListener((delta) => {
  chrome.downloads.search({ id: delta.id }, (items) => {
    if (chrome.runtime.lastError || !items || items.length === 0) return;
    sendDownload(items[0]);
  });
});

async function receiverStatus() {
  try {
    const response = await fetch(receiverUrl("/status"), { cache: "no-store" });
    const body = await response.json();
    if (!response.ok || !body || body.product !== "win-domain-flow") return null;
    return body;
  } catch {
    return null;
  }
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || typeof message.type !== "string") return false;

  if (message.type === "diagnostics-health") {
    Promise.all([
      receiverStatus(),
      chrome.storage.local.get({ diagnosticsEnabled: false, receiverPort: DEFAULT_RECEIVER_PORT })
    ])
      .then(([status, values]) => sendResponse({
        ok: Boolean(status),
        receiverStatus: status,
        enabled: Boolean(values.diagnosticsEnabled),
        receiverPort: validPort(values.receiverPort),
        attachedTabs: attachedTabs.size,
        tabs: Array.from(tabStates.values()),
        queueLength: eventQueue.length,
        droppedEventCount,
        lastAttachError,
        lastReceiverError
      }))
      .catch(() => sendResponse({
        ok: false,
        enabled: diagnosticsEnabled,
        attachedTabs: attachedTabs.size,
        queueLength: eventQueue.length,
        lastReceiverError
      }));
    return true;
  }

  if (message.type === "diagnostics-set-enabled") {
    setDiagnosticsEnabled(Boolean(message.enabled))
      .then(() => sendResponse({
        ok: true,
        enabled: diagnosticsEnabled,
        attachedTabs: attachedTabs.size,
        queueLength: eventQueue.length,
        lastAttachError,
        lastReceiverError
      }))
      .catch((error) => sendResponse({ ok: false, error: String(error) }));
    return true;
  }

  if (message.type === "diagnostics-set-receiver-port") {
    receiverPort = validPort(message.port);
    chrome.storage.local.set({ receiverPort })
      .then(() => {
        processQueue();
        sendResponse({ ok: true, receiverPort });
      })
      .catch((error) => sendResponse({ ok: false, error: String(error) }));
    return true;
  }

  return false;
});

chrome.storage.onChanged.addListener((changes, areaName) => {
  if (areaName !== "local") return;
  if (changes.receiverPort) {
    receiverPort = validPort(changes.receiverPort.newValue);
    processQueue();
  }
  if (changes.diagnosticsEnabled) {
    const next = Boolean(changes.diagnosticsEnabled.newValue);
    if (next !== diagnosticsEnabled) {
      diagnosticsEnabled = next;
      if (next) attachAllTabs(); else detachAllTabs();
    }
  }
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm && alarm.name === RETRY_ALARM) processQueue();
});

async function initialize() {
  const values = await chrome.storage.local.get({
    diagnosticsEnabled: false,
    receiverPort: DEFAULT_RECEIVER_PORT,
    [QUEUE_STORAGE_KEY]: [],
    droppedEventCount: 0
  });
  diagnosticsEnabled = Boolean(values.diagnosticsEnabled);
  receiverPort = validPort(values.receiverPort);
  droppedEventCount = positiveInteger(values.droppedEventCount) || 0;
  const storedQueue = Array.isArray(values[QUEUE_STORAGE_KEY]) ? values[QUEUE_STORAGE_KEY] : [];
  eventQueue = storedQueue
    .filter((entry) => entry && entry.payload && entry.payload.eventId)
    .map((entry) => ({
      ...entry,
      revision: Number(entry.revision) || 0,
      attempts: Math.max(0, Number(entry.attempts) || 0),
      nextAttemptAt: Math.max(0, Number(entry.nextAttemptAt) || 0)
    }));
  const invalidDropped = storedQueue.length - eventQueue.length;
  if (invalidDropped > 0) droppedEventCount += invalidDropped;
  const trimmed = trimQueueToBounds();
  if (invalidDropped > 0 || trimmed > 0) await persistQueue();
  processQueue();
  if (diagnosticsEnabled) await attachAllTabs();
  await updateBadge();
}

chrome.runtime.onInstalled.addListener(() => {
  initialize();
});

chrome.runtime.onStartup.addListener(() => {
  initialize();
});

initialize();

setInterval(() => {
  const cutoff = Date.now() - 30 * 60 * 1000;
  for (const [key, item] of pending.entries()) {
    if ((item.timestampMs || 0) < cutoff) {
      reportRequest(item, "request tracking expired");
      pending.delete(key);
      requestSequences.delete(key);
    }
  }
  processQueue();
}, 60 * 1000);
