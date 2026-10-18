import { invoke } from "./api";
import type { OfficialAccountsResult } from "./App.types";

type Cache = {
  promise?: Promise<OfficialAccountsResult>;
  result?: OfficialAccountsResult;
  expiresAt?: number;
};

const LIST_TTL_MS = 2_000;
let cache: Cache = {};

export function rememberOfficialAccounts(result: OfficialAccountsResult) {
  if (!Array.isArray(result.accounts)) return;
  cache = {
    result,
    expiresAt: Date.now() + LIST_TTL_MS,
  };
}

export function listOfficialAccounts(options?: { force?: boolean }): Promise<OfficialAccountsResult> {
  const force = options?.force === true;
  if (!force && cache.promise) return cache.promise;
  if (!force && cache.result && cache.expiresAt && Date.now() < cache.expiresAt) {
    return Promise.resolve(cache.result);
  }

  const promise: Promise<OfficialAccountsResult> = invoke<OfficialAccountsResult>("list_official_accounts").then(
    (result) => {
      // 强制刷新或账号变更会替换缓存；旧读取的消费者也应取得最新结果。
      if (cache.promise !== promise) return cache.promise ?? cache.result ?? result;
      cache = { result: cache.result, expiresAt: cache.expiresAt };
      rememberOfficialAccounts(result);
      return result;
    },
    (error) => {
      const latest = cache.promise ?? cache.result;
      if (cache.promise !== promise && latest) return latest;
      if (cache.promise === promise) {
        cache = cache.result
          ? { result: cache.result, expiresAt: cache.expiresAt }
          : {};
      }
      throw error;
    },
  );
  cache.promise = promise;
  return promise;
}
