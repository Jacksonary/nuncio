/** Single shared TanStack Query client for the whole app. */
import { QueryClient } from "@tanstack/react-query";
import { isIpcError } from "@/ipc/api";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 30_000,
      gcTime: 5 * 60_000,
      refetchOnWindowFocus: false,
      // Retry network-ish failures once, never retry auth/cancel/validation.
      retry: (failureCount, error) => {
        if (isIpcError(error)) {
          if (
            error.code === "auth" ||
            error.code === "cancelled" ||
            error.code === "invalidInput" ||
            error.code === "notFound" ||
            error.code === "unsupported"
          ) {
            return false;
          }
        }
        return failureCount < 1;
      },
    },
    mutations: {
      retry: false,
    },
  },
});
