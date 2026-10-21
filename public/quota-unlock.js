// Codey 本地第三方线路的发送判断辅助接口。
// 官方额度数据始终原样保留；调用方只在发送判断处使用此接口。
// 旧版曾替换全局 JSON.parse / WebSocket，无法在此安全恢复原函数。
// 升级后须加载新 document，才能移除旧版已安装的拦截。
(() => {
  "use strict";
  if (window.__codeyQuotaUnlock) return;
  let enabled = true;
  try { enabled = localStorage.getItem("codeyQuotaUnlock") !== "off"; } catch {}
  const version = "2.0.0";
  const installedAt = new Date().toISOString();
  window.__codeyQuotaUnlock = {
    version,
    status() { return { version, enabled, installedAt }; },
    allowsLocalSend(context) {
      if (!enabled || !context || context.hostId !== "local" ||
          typeof context.model !== "string" || context.model.trim() === "") return false;
      try {
        return window.__codeyModelWhitelistPatch?.isThirdPartyModel(context) === true;
      } catch { return false; }
    },
    enable() {
      enabled = true;
      try { localStorage.removeItem("codeyQuotaUnlock"); } catch {}
      return "已启用本地第三方线路发送判断";
    },
    disable() {
      enabled = false;
      try { localStorage.setItem("codeyQuotaUnlock", "off"); } catch {}
      return "已停用本地第三方线路发送判断";
    },
  };
})();
