import { CircleArrowUp } from "lucide-react";
import { theme } from "../theme";

/**
 * Contextual action bar pinned to the bottom of the screen. It only appears once
 * at least one image is selected, showing the count plus Clear and Update actions.
 * The Update action is wired to the backend in a later phase.
 */
export default function ActionBar({
  count,
  onClear,
  onUpdate,
}: {
  count: number;
  onClear: () => void;
  onUpdate: () => void;
}) {
  if (count === 0) return null;
  return (
    <div className="fixed inset-x-0 bottom-0 z-20 flex justify-center px-4 pb-6">
      <div
        className={`flex items-center gap-4 rounded-lg border bg-white shadow-lg border-${theme}-300 dark:border-${theme}-800 dark:bg-${theme}-900 px-5 py-3`}
      >
        <span className="text-sm font-medium text-black dark:text-white">
          {count} selected
        </span>
        <button
          onClick={onClear}
          className={`text-sm transition-colors duration-200 text-${theme}-500 hover:text-black dark:hover:text-white`}
        >
          Clear
        </button>
        <button
          onClick={onUpdate}
          className="flex items-center gap-2 rounded-md bg-blue-500 px-3 py-1.5 text-sm font-medium text-white transition-colors duration-200 hover:bg-blue-600"
        >
          <CircleArrowUp className="size-4" />
          Update
        </button>
      </div>
    </div>
  );
}
