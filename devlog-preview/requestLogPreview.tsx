import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";

import "../src/tailwind.css";
import "../src/styles.css";
import "../src/styles.operations.css";
import "../src/styles.models.css";
import "../src/styles.features.css";
import "../src/styles.diagnostics.css";
import "../src/styles.responsive.css";

import "../src/dev/mockApi";
import { UiProvider } from "../src/UiProvider";
import { invoke } from "../src/api";
import { installRequestLogTheme } from "../src/requestLogTheme";
import { RequestLogDialog, type RequestLogCatalog } from "../src/RequestLogDialog";

installRequestLogTheme();
document.title = "Codey 请求日志";

function Preview() {
  const [catalog, setCatalog] = useState<RequestLogCatalog | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    void invoke<{ config: RequestLogCatalog }>("load_codey_config")
      .then((result) => setCatalog(result.config))
      .catch((nextError: unknown) => setError(String(nextError)));
  }, []);

  if (error) return <main className="p-6 text-sm">{error}</main>;
  if (!catalog) return <main className="p-6 text-sm">正在加载请求日志…</main>;
  return <RequestLogDialog catalog={catalog} container={null} opened standalone />;
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <UiProvider>
    <Preview />
  </UiProvider>,
);
