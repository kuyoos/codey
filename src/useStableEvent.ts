import { useCallback, useLayoutEffect, useRef } from "react";

export function useStableEvent<Args extends unknown[], Result>(
  callback: (...args: Args) => Result,
): (...args: Args) => Result {
  const callbackRef = useRef(callback);
  // 使用布局 effect，让子组件的 useEffect 读到最新回调；本应用不使用 SSR。
  useLayoutEffect(() => {
    callbackRef.current = callback;
  }, [callback]);
  return useCallback(
    (...args: Args) => callbackRef.current(...args),
    [],
  );
}
