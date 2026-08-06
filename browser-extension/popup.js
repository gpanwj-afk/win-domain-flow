const enabled = document.getElementById("enabled");
const status = document.getElementById("status");

function renderStatus(ok) {
  status.className = `status ${ok ? "ok" : "bad"}`;
  status.textContent = ok
    ? "本机接收器已连接"
    : "未连接：请先启动域流量管家 GUI";
}

async function refresh() {
  const values = await chrome.storage.local.get({ diagnosticsEnabled: false });
  enabled.checked = Boolean(values.diagnosticsEnabled);
  chrome.runtime.sendMessage({ type: "diagnostics-health" }, (response) => {
    if (chrome.runtime.lastError) {
      renderStatus(false);
      return;
    }
    renderStatus(Boolean(response && response.ok));
  });
}

enabled.addEventListener("change", async () => {
  await chrome.storage.local.set({ diagnosticsEnabled: enabled.checked });
  refresh();
});

refresh();
