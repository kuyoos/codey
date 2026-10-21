import { useCallback, useEffect, useRef, useState } from "react";

import { invoke } from "./api";
import { errorText } from "./appUtils";
import { listOfficialAccounts, rememberOfficialAccounts } from "./officialAccountsRequests";
import type { OfficialAccount, OfficialAccountsResult } from "./App.types";

type Options = {
  onAccountsLoaded?: (accounts: OfficialAccount[] | null) => void;
  onAccountsChanged: (result: OfficialAccountsResult) => void;
};

export function useOfficialAccounts(options: Options) {
  const [accounts, setAccounts] = useState<OfficialAccount[] | null>(null);
  const [loadError, setLoadError] = useState("");
  const callbacks = useRef(options);
  callbacks.current = options;
  const mounted = useRef(true);
  const generation = useRef(0);

  const applyResult = useCallback((result: OfficialAccountsResult) => {
    if (!mounted.current) return;
    // 账号变更成功后，之前发出的列表和线路读取都不能再提交结果。
    generation.current += 1;
    if (Array.isArray(result.accounts)) {
      rememberOfficialAccounts(result);
      setAccounts(result.accounts);
      setLoadError("");
    }
    callbacks.current.onAccountsChanged(result);
  }, []);

  const refresh = useCallback(async (force = false) => {
    if (!mounted.current) return;
    const request = ++generation.current;
    try {
      const result = await listOfficialAccounts({ force });
      if (!mounted.current || request !== generation.current) return;
      const loaded = result.accounts ?? [];
      if (!Array.isArray(loaded)) throw new Error("官方账号列表格式无效，请重新加载");
      setAccounts(loaded);
      callbacks.current.onAccountsLoaded?.(loaded);
      setLoadError("");
    } catch (error) {
      if (!mounted.current || request !== generation.current) return;
      setLoadError(errorText(error));
      callbacks.current.onAccountsLoaded?.(null);
    }
  }, []);

  // 线路刷新同时返回账号、配置和模型，必须与列表读取共用有效性判断。
  const refreshRoutes = useCallback(async () => {
    if (!mounted.current) return;
    const request = ++generation.current;
    try {
      const result = await invoke<OfficialAccountsResult>("refresh_official_account_routes");
      if (!mounted.current || request !== generation.current) return;
      applyResult(result);
      setLoadError("");
    } catch (error) {
      if (!mounted.current || request !== generation.current) return;
      setLoadError(errorText(error));
    }
  }, [applyResult]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    return () => {
      mounted.current = false;
      generation.current += 1;
    };
  }, [refresh]);

  return { accounts, loadError, applyResult, refresh, refreshRoutes };
}
