import { useEffect, useReducer, useState } from "react";

export const REFRESH_MS = 2000;

export function usePolling<T>(
  load: (signal: AbortSignal) => Promise<T>,
  interval: number = REFRESH_MS,
  keepPolling?: (data: T) => boolean,
) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revision, reload] = useReducer((n: number) => n + 1, 0);

  useEffect(() => {
    const controller = new AbortController();
    let timer: number | undefined;
    async function tick() {
      try {
        const current = await load(controller.signal);
        if (controller.signal.aborted) return;
        setData(current);
        setError(null);
        if (keepPolling && !keepPolling(current)) return;
      } catch (err) {
        if (controller.signal.aborted) return;
        setData(null);
        setError(err instanceof Error ? err.message : String(err));
      }
      timer = window.setTimeout(tick, interval);
    }
    void tick();
    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [load, interval, keepPolling, revision]);

  return { data, error, reload };
}
