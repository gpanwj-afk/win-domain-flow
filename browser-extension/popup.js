const enabled = document.getElementById("enabled");
const status = document.getElementById("status");
let changing = false;

function renderStatus(response) {
  const receiverOk = Boolean(response && response.ok);
  const active = Boolean(response && response.enabled);
  const attachedTabs = Number(response && response.attachedTabs) || 0;
  const healthy = receiverOk && (!active || attachedTabs > 0);

  status.className = `status ${healthy ? "ok" : "bad"}`;
  if (!receiverOk) {
    status.textContent = "未连接：请先启动域流量管家 GUI";
  } else if (!active) {
    status.textContent = "本机接收器已连接，诊断尚未启用";
  } else if (attachedTabs > 0) {
    status.textContent = `深度诊断已启用，正在跟踪 ${attachedTabs} 个网页标签页`;
  } else {
    status.textContent = response.lastAttachError
      ? `无法附加网页标签页：${response.lastAttachError}`
      : "已启用，但暂时没有可跟踪的 http/https 网页";
  }
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
