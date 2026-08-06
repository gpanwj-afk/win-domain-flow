const ENDPOINT = "http://127.0.0.1:38765/events";
const HEALTH_ENDPOINT = "http://127.0.0.1:38765/health";
const LOCAL_ENDPOINT_PREFIX = "http://127.0.0.1:38765/";
const PROTOCOL_VERSION = "1.3";
const PARTIAL_REPORT_BYTES = 1024 * 1024;
const PARTIAL_REPORT_MS = 5000;
const pending = new Map();
const attachedTabs = new Set();
let diagnosticsEnabled = false;
let lastAttachError = null;
let lastReceiverError = null;

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
  return !url || url.startsWith(LOCAL_ENDPOINT_PREFIX) || url.startsWith("chrome-extension://") || url.startsWith("edge-extension://");
}

function requestKey(tabId, requestId) {
  return `${tabId}:${requestId}`;
}

function positiveInteger(value) {
  if (!Number.isFinite(value) || value < 0) return null;
  const rounded = Math.round(value);
  return Number.isSafeInteger(rounded) ? rounded : null;
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
    eventId: `cdp:${tabId}:${params.requestId}:${timestampMs}`,
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
    transferredBytes: 0,
    protocol: null,
    fromCache: false,
    contentDisposition: null,
    errorText: null,
    lastReportedBytes: 0,
    lastReportedAt: 0
  };
}

async function sendEvent(payload) {
  if (!diagnosticsEnabled) return false;
  try {
    const response = await fetch(ENDPOINT, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      cache: "no-store"
    });
    if (!response.ok) throw new Error(`local receiver returned ${response.status}`);
    lastReceiverError = null;
    return true;
  } catch (error) {
    lastReceiverError = String(error && error.message ? error.message : error);
    return false;
  }
}

async function reportRequest(item, errorText = null) {
  if (!item || shouldIgnore(item.url) || !item.host) return;
  item.errorText = errorText || item.errorText;
  const transferredBytes = positiveInteger(item.transferredBytes);
  const sent = await sendEvent({
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
    transferredBytes,
    protocol: item.protocol,
    fromCache: item.fromCache,
    contentDisposition: item.contentDisposition,
    errorText: item.errorText
  });
  if (sent) {
    item.lastReportedBytes = item.transferredBytes;
    item.lastReportedAt = Date.now();
  }
}

async function maybeReportPartial(item) {
  const now = Date.now();
  const byteDelta = item.transferredBytes - item.lastReportedBytes;
  const timeDelta = now - item.lastReportedAt;
  if (byteDelta >= PARTIAL_REPORT_BYTES || (item.transferredBytes > 0 && timeDelta >= PARTIAL_REPORT_MS)) {
    await reportRequest(item);
  }
}

async function attachTab(tab) {
  if (!diagnosticsEnabled || !tab || !Number.isInteger(tab.id) || !isInspectableUrl(tab.url) || attachedTabs.has(tab.id)) return;
  const target = { tabId: tab.id };
  try {
    await chrome.debugger.attach(target, PROTOCOL_VERSION);
    await chrome.debugger.sendCommand(target, "Network.enable", {
      maxTotalBufferSize: 0,
      maxResourceBufferSize: 0,
      maxPostDataSize: 0
    });
    attachedTabs.add(tab.id);
    lastAttachError = null;
  } catch (error) {
    lastAttachError = `${tab.title || tab.url || `tab ${tab.id}`}: ${error && error.message ? error.message : error}`;
  }
  await updateBadge();
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
  await Promise.all(tabIds.map(async (tabId) => {
    try {
      await chrome.debugger.detach({ tabId });
    } catch {
      // The tab may already be closed or detached by DevTools.
    }
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
      await chrome.action.setBadgeBackgroundColor({ color: attachedTabs.size > 0 ? "#047857" : "#b45309" });
    }
  } catch {
    // Badge support is non-critical.
  }
}

chrome.debugger.onEvent.addListener((source, method, params = {}) => {
  const tabId = source.tabId;
  if (!diagnosticsEnabled || !Number.isInteger(tabId)) return;
  const key = params.requestId ? requestKey(tabId, params.requestId) : null;

  if (method === "Network.requestWillBeSent") {
    if (shouldIgnore(params.request && params.request.url)) return;
    pending.set(key, baseRequest(tabId, params));
    return;
  }

  if (!key) return;
  const item = pending.get(key);
  if (!item) return;

  if (method === "Network.responseReceived") {
    const response = params.response || {};
    item.statusCode = positiveInteger(response.status);
    item.mime = cleanMime(response.mimeType || headerValue(response.headers, "content-type"));
    item.declaredBytes = parseDeclaredBytes(response.headers);
    item.contentDisposition = headerValue(response.headers, "content-disposition");
    item.protocol = response.protocol || null;
    item.fromCache = Boolean(response.fromDiskCache || response.fromServiceWorker || response.fromPrefetchCache);
    item.resourceType = String(params.type || item.resourceType || "other").toLowerCase();
    return;
  }

  if (method === "Network.requestServedFromCache") {
    item.fromCache = true;
    return;
  }

  if (method === "Network.dataReceived") {
    const increment = positiveInteger(params.encodedDataLength) ?? positiveInteger(params.dataLength) ?? 0;
    item.transferredBytes += increment;
    maybeReportPartial(item);
    return;
  }

  if (method === "Network.loadingFinished") {
    const total = positiveInteger(params.encodedDataLength);
    if (total !== null) item.transferredBytes = Math.max(item.transferredBytes, total);
    pending.delete(key);
    reportRequest(item);
    return;
  }

  if (method === "Network.loadingFailed") {
    pending.delete(key);
    reportRequest(item, params.errorText || "request failed");
  }
});

chrome.debugger.onDetach.addListener((source, reason) => {
  if (!Number.isInteger(source.tabId)) return;
  attachedTabs.delete(source.tabId);
  for (const key of pending.keys()) {
    if (key.startsWith(`${source.tabId}:`)) pending.delete(key);
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
  if (changeInfo.url || changeInfo.status === "loading" || changeInfo.status === "complete") {
    attachTab({ ...tab, id: tabId });
  }
});

chrome.tabs.onRemoved.addListener((tabId) => {
  attachedTabs.delete(tabId);
  for (const key of pending.keys()) {
    if (key.startsWith(`${tabId}:`)) pending.delete(key);
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
  await sendEvent({
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

async function receiverHealth() {
  try {
    const response = await fetch(HEALTH_ENDPOINT, { cache: "no-store" });
    return response.ok;
  } catch {
    return false;
  }
}

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || typeof message.type !== "string") return false;

  if (message.type === "diagnostics-health") {
    Promise.all([receiverHealth(), chrome.storage.local.get({ diagnosticsEnabled: false })])
      .then(([receiverOk, values]) => sendResponse({
        ok: receiverOk,
        enabled: Boolean(values.diagnosticsEnabled),
        attachedTabs: attachedTabs.size,
        lastAttachError,
        lastReceiverError
      }))
      .catch(() => sendResponse({ ok: false, enabled: diagnosticsEnabled, attachedTabs: attachedTabs.size }));
    return true;
  }

  if (message.type === "diagnostics-set-enabled") {
    setDiagnosticsEnabled(Boolean(message.enabled))
      .then(() => sendResponse({ ok: true, enabled: diagnosticsEnabled, attachedTabs: attachedTabs.size, lastAttachError }))
      .catch((error) => sendResponse({ ok: false, error: String(error) }));
    return true;
  }

  return false;
});

chrome.storage.onChanged.addListener((changes, areaName) => {
  if (areaName !== "local" || !changes.diagnosticsEnabled) return;
  const next = Boolean(changes.diagnosticsEnabled.newValue);
  if (next !== diagnosticsEnabled) {
    diagnosticsEnabled = next;
    if (next) attachAllTabs(); else detachAllTabs();
  }
});

async function initialize() {
  const values = await chrome.storage.local.get({ diagnosticsEnabled: false });
  diagnosticsEnabled = Boolean(values.diagnosticsEnabled);
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
    }
  }
}, 60 * 1000);
