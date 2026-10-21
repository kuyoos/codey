import { useEffect, useRef } from "react";

import { invoke } from "./api";
import { errorText } from "./appUtils";
import type { OfficialAccountsResult } from "./App.types";

type LoginPoll = OfficialAccountsResult & {
  status: "wait" | "ok" | "failed" | "expired";
  message?: string;
};

type Options = {
  loginId?: string;
  command: "poll_official_account_login" | "poll_official_account_siwc";
  onSuccess: (result: OfficialAccountsResult) => void;
  onError: (message: string) => void;
  expiredMessage: string;
  failedMessage: string;
};

export function useOfficialAccountLoginPoll(options: Options) {
  const callbacks = useRef(options);
  callbacks.current = options;
  const { loginId, command } = options;
  useEffect(() => {
    if (!loginId) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      if (!active) return;
      try {
        const result = await invoke<LoginPoll>(command, { loginId });
        if (!active) return;
        if (result.status === "wait") {
          timer = setTimeout(() => void poll(), 1500);
        } else if (result.status === "ok") {
          callbacks.current.onSuccess(result);
        } else {
          callbacks.current.onError(result.message || (result.status === "expired"
            ? callbacks.current.expiredMessage : callbacks.current.failedMessage));
        }
      } catch (error) {
        if (active) callbacks.current.onError(errorText(error));
      }
    };
    void poll();
    return () => {
      active = false;
      if (timer !== undefined) clearTimeout(timer);
    };
  }, [loginId, command]);
}
