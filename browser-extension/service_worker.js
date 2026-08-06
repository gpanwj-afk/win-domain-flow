const ENDPOINT = "http://127.0.0.1:38765/events";
const HEALTH_ENDPOINT = "http://127.0.0.1:38765/health";
const LOCAL_ENDPOINT_PREFIX = "http://127.0.0.1:38765/";
const pending = new Map();

async function isEnabled() {
  const values = await chrome.storage.local.get({ diagnosticsEnabled: false });
  return Boolean(values.diagnosticsEnabled);
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

function shouldIgnore(url) {
  return !url || url.startsWith(LOCAL_ENDPOINT_PREFIX) || url.startsWith("chrome-extension://") || url.startsWith("edge-extension://");
}

function headerValue(headers, name) {
  if (!Array.isArray(headers)) return null;
  const match = headers.find((header) => String(header.name || "").toLowerCase() === name.toLowerCase());
  return match && typeof match.value === "string" ? match.value : null;
}

function parseDeclaredBytes(headers) {
  const contentLength = headerValue(headers, "content-length");
  if (contentLength && /^\d+$/.test(contentLength.trim())) {
    const value = Number(contentLength.trim());
    return Number.isSafeInteger(value) && value >= 0 ? value : null;
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
  return value ? value.split(";", 1)[0].trim().toLowerCase() : null;
}

async function sendEvent(payload) {
  if (!(await isEnabled())) return;
  try {
    await fetch(ENDPOINT, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      cache: "no-store"
    });
  } catch {
    // The desktop app may not be running. The extension intentionally stays quiet.
  }
}

function baseRequest(details) {
  return {
    requestId: details.requestId,
    timestampMs: Math.round(details.timeStamp || Date.now()),
    url: details.url,
    host: hostOf(details.url),
    pageUrl: details.documentUrl || null,
    initiator: details.initiator || null,
    method: details.method || null,
    resourceType: details.type || "other",
    statusCode: null,
    mime: null,
    declaredBytes: null,
    contentDisposition: null,
    errorText: null
  };
}

chrome.webRequest.onBeforeRequest.addListener(
  (details) => {
    if (shouldIgnore(details.url)) return;
    pending.set(details.requestId, baseRequest(details));
  },
  { urls: ["<all_urls>"] }
);

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (shouldIgnore(details.url)) return;
    const item = pending.get(details.requestId) || baseRequest(details);
    item.timestampMs = Math.round(details.timeStamp || item.timestampMs || Date.now());
    item.statusCode = Number.isFinite(details.statusCode) ? details.statusCode : null;
    item.mime = cleanMime(headerValue(details.responseHeaders, "content-type"));
    item.declaredBytes = parseDeclaredBytes(details.responseHeaders);
    item.contentDisposition = headerValue(details.responseHeaders, "content-disposition");
    item.pageUrl = item.pageUrl || details.documentUrl || null;
    item.initiator = item.initiator || details.initiator || null;
    pending.set(details.requestId, item);
  },
  { urls: ["<all_urls>"] },
  ["responseHeaders"]
);

chrome.webRequest.onCompleted.addListener(
  (details) => {
    if (shouldIgnore(details.url)) return;
    const item = pending.get(details.requestId) || baseRequest(details);
    pending.delete(details.requestId);
    item.timestampMs = Math.round(details.timeStamp || item.timestampMs || Date.now());
    item.statusCode = Number.isFinite(details.statusCode) ? details.statusCode : item.statusCode;
    item.pageUrl = item.pageUrl || details.documentUrl || null;
    item.initiator = item.initiator || details.initiator || null;
    sendEvent({
      kind: "request",
      eventId: `request:${details.requestId}:${item.timestampMs}`,
      ...item
    });
  },
  { urls: ["<all_urls>"] }
);

chrome.webRequest.onErrorOccurred.addListener(
  (details) => {
    if (shouldIgnore(details.url)) return;
    const item = pending.get(details.requestId) || baseRequest(details);
    pending.delete(details.requestId);
    item.timestampMs = Math.round(details.timeStamp || item.timestampMs || Date.now());
    item.errorText = details.error || "request failed";
    sendEvent({
      kind: "request",
      eventId: `request:${details.requestId}:${item.timestampMs}`,
      ...item
    });
  },
  { urls: ["<all_urls>"] }
);

function toMillis(value) {
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

async function sendDownload(item) {
  if (!item || shouldIgnore(item.finalUrl || item.url)) return;
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

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || message.type !== "diagnostics-health") return false;
  fetch(HEALTH_ENDPOINT, { cache: "no-store" })
    .then((response) => sendResponse({ ok: response.ok }))
    .catch(() => sendResponse({ ok: false }));
  return true;
});

setInterval(() => {
  const cutoff = Date.now() - 5 * 60 * 1000;
  for (const [requestId, item] of pending.entries()) {
    if ((item.timestampMs || 0) < cutoff) pending.delete(requestId);
  }
}, 60 * 1000);
