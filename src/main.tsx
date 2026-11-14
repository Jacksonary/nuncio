/**
 * React entry point.
 * Theme + accent + language are restored from the cached settings *before*
 * the first paint so there is no light/dark flash in the transparent window.
 */
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import App from "@/App";
import { queryClient } from "@/lib/queryClient";
import { bootstrapTheme } from "@/lib/theme";
import "@/styles/tokens.css";
import "@/styles/global.css";

bootstrapTheme();

const container = document.getElementById("root");
if (!container) {
  throw new Error("Nuncio: #root element is missing from index.html");
}

createRoot(container).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
