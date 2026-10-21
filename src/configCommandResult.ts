import type { Dispatch, SetStateAction } from "react";
import type { Config, ConfigApplyStatus, ModelState, ProviderStatus, RuntimeStatus } from "./App.types";

export type ConfigCommandResult = {
  config: Config;
  configApply?: ConfigApplyStatus | null;
  providerStatus?: ProviderStatus;
  modelState?: ModelState;
  restartRequired?: boolean;
};

type Options = {
  adoptConfig: (config: Config) => { dirty: boolean } | null;
  acceptConfigApply: (status?: ConfigApplyStatus | null) => ConfigApplyStatus | null | undefined;
  setDirty: (dirty: boolean) => void;
  setProviderStatus: (status: ProviderStatus) => void;
  setModelState: (state: ModelState) => void;
  setStatus: Dispatch<SetStateAction<RuntimeStatus>>;
};

// 配置与随附状态作为同一份结果接纳，旧修订不能只更新模型或运行状态。
export function applyConfigCommandResult(result: ConfigCommandResult, options: Options): boolean {
  const merged = options.adoptConfig(result.config);
  if (!merged) return false;
  result.configApply = options.acceptConfigApply(result.configApply);
  options.setDirty(merged.dirty);
  if (result.providerStatus) options.setProviderStatus(result.providerStatus);
  if (result.modelState) options.setModelState(result.modelState);
  if (!result.configApply && typeof result.restartRequired === "boolean") {
    options.setStatus((current) => {
      // 后台应用状态可能先于命令响应返回，不能再用旧的兼容字段覆盖它。
      if (current.configApply && current.configApply.revision >= result.config.settingsRevision) return current;
      return { ...current, restartRequired: result.restartRequired };
    });
  }
  return true;
}
