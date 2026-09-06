import { useState } from "react";
import {
  Dialog,
  DialogBackdrop,
  DialogPanel,
  DialogTitle,
} from "@headlessui/react";
import { CircleCheck, Loader2, TriangleAlert, X } from "lucide-react";
import { theme } from "../theme";

interface UpdateResult {
  reference: string;
  project: string;
  working_dir: string;
  success: boolean;
  message: string;
}

const endpoint =
  process.env.NODE_ENV === "production"
    ? "./actions/update"
    : `http://${window.location.hostname}:8000/actions/update`;

/**
 * Confirm → run → results flow for applying updates. Mounted only while open, so it
 * always starts fresh at the confirm step. `onClose(didUpdate)` lets the parent reload
 * the page after a real update so the new statuses show.
 */
export default function UpdateDialog({
  references,
  onClose,
}: {
  references: string[];
  onClose: (didUpdate: boolean) => void;
}) {
  const [phase, setPhase] = useState<"confirm" | "running" | "done">("confirm");
  const [results, setResults] = useState<UpdateResult[]>([]);
  const [error, setError] = useState<string | null>(null);

  const runUpdate = async () => {
    setPhase("running");
    setError(null);
    try {
      const res = await fetch(endpoint, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ references }),
      });
      if (!res.ok) throw new Error(`Server returned ${res.status}`);
      const data = (await res.json()) as { results: UpdateResult[] };
      setResults(data.results ?? []);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Request failed");
    } finally {
      setPhase("done");
    }
  };

  return (
    <Dialog
      open
      onClose={() => {
        if (phase !== "running") onClose(phase === "done");
      }}
      className="relative z-30"
    >
      <DialogBackdrop
        className={`fixed inset-0 bg-${theme}-500 dark:bg-${theme}-950 !bg-opacity-75`}
      />
      <div className="fixed inset-0 z-10 w-screen overflow-y-auto">
        <div className="flex min-h-full items-end justify-center sm:items-center sm:p-0">
          <DialogPanel
            className={`relative w-full overflow-hidden rounded-t-lg bg-white shadow-xl dark:border dark:border-${theme}-800 dark:bg-${theme}-900 sm:my-8 sm:max-w-lg sm:rounded-lg dark:text-white`}
          >
            <div className="flex flex-col gap-4 px-6 py-5">
              <div className="flex items-center justify-between">
                <DialogTitle className="text-lg font-semibold text-black dark:text-white">
                  {phase === "confirm" && "Apply updates?"}
                  {phase === "running" && "Updating…"}
                  {phase === "done" && "Update results"}
                </DialogTitle>
                {phase !== "running" && (
                  <button onClick={() => onClose(phase === "done")}>
                    <X
                      className={`size-6 text-${theme}-500 transition-colors hover:text-black dark:hover:text-white`}
                    />
                  </button>
                )}
              </div>

              {phase === "confirm" && (
                <>
                  <p className={`text-sm text-${theme}-600 dark:text-${theme}-400`}>
                    This runs <code>docker compose pull &amp;&amp; up -d</code> for{" "}
                    {references.length} image
                    {references.length === 1 ? "" : "s"}, recreating the affected
                    container(s):
                  </p>
                  <ul className="flex flex-col gap-1 font-mono text-sm">
                    {references.map((r) => (
                      <li key={r} className="break-all">
                        • {r}
                      </li>
                    ))}
                  </ul>
                  <div className="mt-2 flex justify-end gap-2">
                    <button
                      onClick={() => onClose(false)}
                      className={`rounded-md px-3 py-1.5 text-sm text-${theme}-600 transition-colors hover:text-black dark:text-${theme}-400 dark:hover:text-white`}
                    >
                      Cancel
                    </button>
                    <button
                      onClick={runUpdate}
                      className="rounded-md bg-blue-500 px-3 py-1.5 text-sm font-medium text-white transition-colors hover:bg-blue-600"
                    >
                      Update {references.length}
                    </button>
                  </div>
                </>
              )}

              {phase === "running" && (
                <div
                  className={`flex items-center gap-3 py-2 text-${theme}-600 dark:text-${theme}-400`}
                >
                  <Loader2 className="size-5 animate-spin" />
                  Pulling images and recreating containers…
                </div>
              )}

              {phase === "done" && (
                <>
                  {error && (
                    <div className="flex items-center gap-3 rounded-md bg-yellow-400/10 px-3 py-2">
                      <TriangleAlert className="size-5 shrink-0 text-yellow-500" />
                      {error}
                    </div>
                  )}
                  <ul className="flex flex-col gap-3">
                    {results.map((r) => (
                      <li key={r.reference} className="flex items-start gap-3">
                        {r.success ? (
                          <CircleCheck className="mt-0.5 size-5 shrink-0 text-green-500" />
                        ) : (
                          <TriangleAlert className="mt-0.5 size-5 shrink-0 text-red-500" />
                        )}
                        <div className="min-w-0">
                          <div className="break-all font-mono text-sm">
                            {r.reference}
                          </div>
                          <div
                            className={`break-words text-xs text-${theme}-500`}
                          >
                            {r.message}
                          </div>
                        </div>
                      </li>
                    ))}
                  </ul>
                  {!error && results.length === 0 && (
                    <p className={`text-sm text-${theme}-500`}>Nothing to update.</p>
                  )}
                  <div className="mt-2 flex justify-end">
                    <button
                      onClick={() => onClose(true)}
                      className="rounded-md bg-blue-500 px-3 py-1.5 text-sm font-medium text-white transition-colors hover:bg-blue-600"
                    >
                      Done
                    </button>
                  </div>
                </>
              )}
            </div>
          </DialogPanel>
        </div>
      </div>
    </Dialog>
  );
}
