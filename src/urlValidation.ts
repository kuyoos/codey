/** Validates optional per-route outbound proxy URLs before they are saved. */
export function validateOutboundProxyUrl(value: string, label = "上游代理") {
  const normalized = value.trim();
  if (!normalized) return "";
  try {
    const parsed = new URL(normalized);
    if (
      !["http:", "https:", "socks5:", "socks5h:"].includes(parsed.protocol) ||
      !parsed.hostname
    ) {
      return `${label}必须是 http、https、socks5 或 socks5h 地址`;
    }
    return "";
  } catch {
    return `${label}格式无效`;
  }
}

/** Validates the Codex launch proxy before it is saved. Stricter than route
 * proxies: credentials are injected by the local auth relay (Chromium's
 * --proxy-server cannot carry them) and SOCKS rules require an explicit port. */
export function validateCodexLaunchProxy(value: string, label = "Codex 启动代理") {
  const normalized = value.trim();
  if (!normalized) return "";
  try {
    const parsed = new URL(normalized);
    if (
      !["http:", "https:", "socks5:", "socks5h:"].includes(parsed.protocol) ||
      !parsed.hostname
    ) {
      return `${label}必须是 http、https、socks5 或 socks5h 地址`;
    }
    // 非特殊协议（socks5/socks5h）的 pathname 是空串而非 "/"。
    if (!["", "/"].includes(parsed.pathname) || parsed.search || parsed.hash) {
      return `${label}只能包含协议、地址、端口和认证信息`;
    }
    if (
      (parsed.protocol === "socks5:" || parsed.protocol === "socks5h:") &&
      !parsed.port
    ) {
      return `${label}的 SOCKS 地址必须带端口`;
    }
    // RFC 1929 的用户名和密码各限 255 字节；http/https 代理没有这个限制，
    // 后端也只对 SOCKS 地址做同样约束。
    if (
      (parsed.protocol === "socks5:" || parsed.protocol === "socks5h:") &&
      (credentialByteLength(parsed.username) > 255 || credentialByteLength(parsed.password) > 255)
    ) {
      return `${label}的 SOCKS 用户名或密码不能超过 255 字节`;
    }
    return "";
  } catch {
    return `${label}格式无效`;
  }
}

function credentialByteLength(value: string) {
  try {
    return new TextEncoder().encode(decodeURIComponent(value)).length;
  } catch {
    return value.length;
  }
}

/** Optional gateway address. Blank keeps the caller's default endpoint. */
export function validateOptionalOutboundApiUrl(value: string, label = "网关地址") {
  const normalized = value.trim();
  if (!normalized) return "";
  return validateOutboundApiUrl(normalized, label);
}

/** Validates custom API endpoints before they are saved. */
export function validateOutboundApiUrl(value: string, label = "API URL") {
  const normalized = value.trim();
  if (!normalized) return `请输入 ${label}`;
  try {
    const parsed = new URL(normalized);
    if (!["http:", "https:"].includes(parsed.protocol) || !parsed.hostname) {
      return `${label}必须是有效的 HTTP(S) 地址`;
    }
    if (parsed.username || parsed.password) {
      return `${label}不能包含用户名或密码，请在 Key 字段单独填写凭据`;
    }
    return "";
  } catch {
    return `${label}格式无效`;
  }
}
