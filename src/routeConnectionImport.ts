/** New API 令牌「复制连接串」生成的剪贴板 JSON。 */
export type NewApiConnectionInfo = {
  key: string;
  url: string;
};

const NEW_API_CONNECTION_TYPE = "newapi_channel_conn";

function parseJsonRecord(text: string): Record<string, unknown> | null {
  try {
    const value: unknown = JSON.parse(text);
    return value !== null && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

/**
 * 解析 New API 连接信息，返回 { key, url }；无法识别时返回 null。
 * 容忍整体首尾空白，以及 JSON 前后夹杂其它文本的粘贴内容。
 */
export function parseNewApiConnectionInfo(
  text: string | null | undefined,
): NewApiConnectionInfo | null {
  if (typeof text !== "string") return null;
  const trimmed = text.trim();
  if (!trimmed) return null;
  let parsed = parseJsonRecord(trimmed);
  if (!parsed) {
    const start = trimmed.indexOf("{");
    const end = trimmed.lastIndexOf("}");
    if (start >= 0 && end > start) {
      parsed = parseJsonRecord(trimmed.slice(start, end + 1));
    }
  }
  if (!parsed || parsed._type !== NEW_API_CONNECTION_TYPE) return null;
  const { key, url } = parsed;
  if (typeof key !== "string" || typeof url !== "string") return null;
  const normalizedKey = key.trim();
  const normalizedUrl = url.trim();
  return normalizedKey && normalizedUrl
    ? { key: normalizedKey, url: normalizedUrl }
    : null;
}

/** 按连接地址推导线路名与短名称建议；推导不出时对应字段返回空串。 */
export function suggestRouteNamesFromUrl(
  url: string,
  maxNameLength: number,
  maxShortNameLength: number,
): { name: string; shortName: string } {
  let hostname = "";
  try {
    hostname = new URL(url).hostname;
  } catch {
    return { name: "", shortName: "" };
  }
  if (!hostname) return { name: "", shortName: "" };
  const labels = hostname.split(".");
  const base = labels.length > 1 && labels[0] === "api" ? labels[1] : labels[0];
  const shortName = Array.from(base.replace(/[^\p{L}\p{N}]/gu, ""))
    .slice(0, maxShortNameLength)
    .join("");
  const name = Array.from(hostname).length <= maxNameLength ? hostname : "";
  return { name, shortName };
}
