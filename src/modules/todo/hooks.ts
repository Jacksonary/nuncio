/** Small React hooks shared by the To Do panes. */
import { useEffect, useState } from "react";
import type { IsoDate } from "@/ipc/types";
import { toIsoDate } from "@/lib/format";
import { useAppSettings } from "@/lib/useSettings";

/** Local calendar date, refreshed at midnight-ish (every minute). */
export function useToday(): IsoDate {
  const [today, setToday] = useState<IsoDate>(() => toIsoDate(new Date()));
  useEffect(() => {
    const timer = window.setInterval(() => {
      const next = toIsoDate(new Date());
      setToday((current) => (current === next ? current : next));
    }, 60_000);
    return () => window.clearInterval(timer);
  }, []);
  return today;
}

/** Poll interval for task/list queries derived from settings (0 = disabled). */
export function useTaskPollIntervalMs(): number | false {
  const settings = useAppSettings();
  const minutes = settings.tasks.pollMinutes;
  return minutes > 0 ? minutes * 60_000 : false;
}
