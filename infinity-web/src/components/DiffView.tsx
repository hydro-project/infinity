import { useMemo, useCallback, useRef, useState } from "react";
import { PatchDiff } from "@pierre/diffs/react";
import { FileTree } from "@pierre/trees/react";
import type { GitStatusEntry } from "@pierre/trees";
import css from "./DiffView.module.css";

interface DiffViewProps {
  diff: string;
  theme: "light" | "dark";
}

interface FilePatch {
  path: string;
  status: "added" | "deleted" | "modified";
  patch: string;
}

/** Sort file patches to match @pierre/trees default order:
 *  folders before files, dot-prefixed before others, case-insensitive alpha. */
function sortPatches(patches: FilePatch[]): FilePatch[] {
  const allPaths = new Set(patches.map((p) => p.path));
  // Collect all directory prefixes
  const dirs = new Set<string>();
  for (const p of allPaths) {
    const parts = p.split("/");
    for (let i = 1; i < parts.length; i++)
      dirs.add(parts.slice(0, i).join("/"));
  }

  return [...patches].sort((a, b) => {
    const aParts = a.path.split("/");
    const bParts = b.path.split("/");
    const len = Math.min(aParts.length, bParts.length);
    for (let i = 0; i < len; i++) {
      if (aParts[i] === bParts[i]) continue;
      const aFull = aParts.slice(0, i + 1).join("/");
      const bFull = bParts.slice(0, i + 1).join("/");
      const aIsDir = dirs.has(aFull) || i < aParts.length - 1;
      const bIsDir = dirs.has(bFull) || i < bParts.length - 1;
      if (aIsDir !== bIsDir) return aIsDir ? -1 : 1;
      const aIsDot = aParts[i].startsWith(".");
      const bIsDot = bParts[i].startsWith(".");
      if (aIsDot !== bIsDot) return aIsDot ? -1 : 1;
      return aParts[i].toLowerCase().localeCompare(bParts[i].toLowerCase());
    }
    return aParts.length - bParts.length;
  });
}

function splitDiff(diff: string): FilePatch[] {
  const results: FilePatch[] = [];
  const parts = diff.split(/^(?=diff --git )/m);
  for (const part of parts) {
    const match = part.match(/^diff --git a\/(.*) b\/(.*)$/m);
    if (!match) continue;
    const oldPath = match[1];
    const path = match[2];
    let status: FilePatch["status"] = "modified";
    if (part.includes("--- /dev/null")) status = "added";
    else if (part.includes("+++ /dev/null") || oldPath !== path)
      status = "deleted";
    results.push({ path, status, patch: part.trimEnd() });
  }
  return results;
}

export function DiffView({ diff, theme }: DiffViewProps) {
  const paneRef = useRef<HTMLDivElement>(null);
  const fileRefs = useRef<Map<string, HTMLDivElement>>(new Map());

  const patches = useMemo(() => sortPatches(splitDiff(diff)), [diff]);
  const filePaths = useMemo(() => patches.map((f) => f.path), [patches]);
  const gitStatus: GitStatusEntry[] = useMemo(
    () => patches.map((f) => ({ path: f.path, status: f.status })),
    [patches],
  );

  const [expandedItems, setExpandedItems] = useState<string[]>();
  const [selectedItems, setSelectedItems] = useState<string[]>([]);

  const handleSelect = useCallback((paths: string[]) => {
    setSelectedItems(paths);
    const el = paths[0] && fileRefs.current.get(paths[0]);
    if (el) el.scrollIntoView({ behavior: "smooth", block: "start" });
  }, []);

  const setFileRef = useCallback(
    (path: string) => (el: HTMLDivElement | null) => {
      if (el) fileRefs.current.set(path, el);
      else fileRefs.current.delete(path);
    },
    [],
  );

  if (!diff) {
    return <div className={css.empty}>No changes yet</div>;
  }

  return (
    <div className={css.root}>
      <div className={css.sidebar}>
        <div className={css.sidebarHeader}>Files</div>
        <FileTree
          files={filePaths}
          gitStatus={gitStatus}
          options={{ flattenEmptyDirectories: true }}
          selectedItems={selectedItems}
          expandedItems={expandedItems}
          onExpandedItemsChange={setExpandedItems}
          onSelectedItemsChange={handleSelect}
        />
      </div>
      <div className={css.diffPane} ref={paneRef}>
        {patches.map((f) => (
          <div className={css.diffPatch} key={f.path} ref={setFileRef(f.path)}>
            <PatchDiff
              patch={f.patch}
              options={{ diffStyle: "unified", themeType: theme ?? "system" }}
            />
          </div>
        ))}
      </div>
    </div>
  );
}
