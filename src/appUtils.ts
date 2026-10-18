export const errorText = (error: unknown) =>
  error instanceof Error ? error.message : String(error);

export function withTimeout<T>(
  promise: Promise<T>,
  timeoutMs: number,
  message: string,
): Promise<T>;
export function withTimeout<T>(
  promise: Promise<T>,
  mutation?: boolean,
  timeoutMs?: number,
): Promise<T>;
export function withTimeout<T>(
  promise: Promise<T>,
  timeoutOrMutation: number | boolean = false,
  messageOrTimeout: string | number = 15000,
): Promise<T> {
  const timeoutMs = typeof timeoutOrMutation === "number"
    ? timeoutOrMutation
    : typeof messageOrTimeout === "number" ? messageOrTimeout : 15000;
  const message = typeof messageOrTimeout === "string"
    ? messageOrTimeout
    : timeoutOrMutation
      ? "操作等待超时，后台可能仍在执行，结果尚不确定。请刷新确认后再操作，避免重复提交。"
      : "读取超时，请检查服务连接后重试。";
  const clock = typeof window !== "undefined" && typeof window.setTimeout === "function"
    ? window
    : globalThis;
  let timer: ReturnType<typeof setTimeout>;
  return new Promise<T>((resolve, reject) => {
    timer = clock.setTimeout(
      () => reject(new Error(message)),
      timeoutMs,
    );
    promise.then(resolve, reject);
  }).finally(() => clock.clearTimeout(timer));
}
