import type { ConfigApplyStatus, Notice } from "./App.types";

export function reconcileConfigApply(
  current: ConfigApplyStatus | null | undefined,
  incoming: ConfigApplyStatus | null | undefined,
): ConfigApplyStatus | null | undefined {
  if (!incoming) return current;
  if (current && (incoming.revision < current.revision ||
    (incoming.revision === current.revision && current.state !== "pending"))) return current;
  return incoming;
}

export function configApplyNotice(status: ConfigApplyStatus): Notice {
  switch (status.state) {
    case "pending": return { tone: "info", text: "配置已保存，正在应用" };
    case "applied": return { tone: "success", text: "配置已保存并应用" };
    case "restart_required": return { tone: "info", text: "配置已保存，部分设置需重启 Codex 后生效" };
    case "failed": return { tone: "error", text: `配置已保存，但未能完全应用：${status.error || "请检查 Codex 状态后重试"}` };
    case "saved": return { tone: "success", text: "配置已保存" };
  }
}

export function configApplyPending(next: ConfigApplyStatus | null | undefined, revision: number) {
  return !next || next.revision < revision || next.state === "pending";
}
