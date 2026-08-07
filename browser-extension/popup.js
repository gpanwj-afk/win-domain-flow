const enabled = document.getElementById("enabled");
const status = document.getElementById("status");
let changing = false;

function renderStatus(response) {
  const receiverOk = Boolean(response && response.ok);
  const active = Boolean(response && response.enabled);
  const attachedTabs = Number(response && response.attachedTabs) || 0;
  const queueLength = Number(response && response.queueLength) || 0;
  const dropped = Number(response && response.droppedEventCount) || 0;
  const receiverError = response && response.lastReceiverError ? String(response.lastReceiverError) : "";
  const tabStates = Array.isArray(response && response.tabs) ? response.tabs : [];
  const failedTabs = tabStates.filter((item) => item && item.lastAttachError);
  const healthy = receiverOk && !receiverError && (!active || attachedTabs > 0) && dropped === 0;

  status.className = `status ${healthy ? "ok" : "bad"}`;
  const lines = [];
  if (!receiverOk) {
    lines.push("未连接：请先启动域流量管家 GUI");
  } else if (!active) {
    lines.push("本机接收器已连接，诊断尚未启用");
  } else if (attachedTabs > 0) {
    lines.push(`深度诊断已启用，正在跟踪 ${attachedTabs} 个网页标签页`);
  } else {
    lines.push(response.lastAttachError
      ? `无法附加网页标签页：${response.lastAttachError}`
      : "已启用，但暂时没有可跟踪的 http/https 网页");
  }

  if (queueLength > 0) lines.push(`待补发事件：${queueLength} 条`);
  if (receiverError) lines.push(`Receiver：${receiverError}`);
  if (failedTabs.length > 0) lines.push(`附加失败标签页：${failedTabs.length} 个`);
  if (dropped > 0) lines.push(`警告：发送队列曾丢弃 ${dropped} 条最旧事件`);

  status.textContent = lines.join("\n");
  status.style.whiteSpace = "pre-line";
}

function refresh() {
  chrome.runtime.sendMessage({ type: "diagnostics-health" }, (response) => {
    if (chrome.runtime.lastError) {
      renderStatus({ ok: false });
      return;
    }
    enabled.checked = Boolean(response && response.enabled);
    renderStatus(response || { ok: false });
  });
}

enabled.addEventListener("change", () => {
  if (changing) return;
  changing = true;
  enabled.disabled = true;
  chrome.runtime.sendMessage(
    { type: "diagnostics-set-enabled", enabled: enabled.checked },
    (response) => {
      changing = false;
      enabled.disabled = false;
      if (chrome.runtime.lastError || !response || !response.ok) {
        enabled.checked = !enabled.checked;
      }
      refresh();
    }
  );
});

refresh();
