import { useMemo, useCallback, useRef, useState, memo } from "react";
import { MultiFileDiff } from "@pierre/diffs/react";
import { FileTree } from "@pierre/trees/react";
import type { GitStatusEntry } from "@pierre/trees";
import css from "./DiffView.module.css";

interface FileEntry {
  path: string;
  status: "added" | "deleted" | "modified";
  oldContents: string;
  newContents: string;
}

interface DiffViewProps {
  files: FileEntry[];
  theme: "light" | "dark";
}

/** Sort file entries: folders before files, dot-prefixed before others, case-insensitive alpha. */
function sortFiles(files: FileEntry[]): FileEntry[] {
  const allPaths = new Set(files.map((f) => f.path));
  const dirs = new Set<string>();
  for (const p of allPaths) {
    const parts = p.split("/");
    for (let i = 1; i < parts.length; i++)
      dirs.add(parts.slice(0, i).join("/"));
  }

  return [...files].sort((a, b) => {
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

const FileDiffEntry = memo(function FileDiffEntry({
  path,
  oldContents,
  newContents,
  options,
}: {
  path: string;
  oldContents: string;
  newContents: string;
  options: Parameters<typeof MultiFileDiff>[0]["options"];
}) {
  const oldFile = useMemo(
    () => ({ name: path, contents: oldContents }),
    [path, oldContents],
  );
  const newFile = useMemo(
    () => ({ name: path, contents: newContents }),
    [path, newContents],
  );
  return (
    <MultiFileDiff oldFile={oldFile} newFile={newFile} options={options} />
  );
});

export const DiffView = memo(function DiffView({
  files,
  theme,
}: DiffViewProps) {
  const paneRef = useRef<HTMLDivElement>(null);
  const fileRefs = useRef<Map<string, HTMLDivElement>>(new Map());

  const sorted = useMemo(() => sortFiles(files), [files]);
  const filePaths = useMemo(() => sorted.map((f) => f.path), [sorted]);
  const gitStatus: GitStatusEntry[] = useMemo(
    () => sorted.map((f) => ({ path: f.path, status: f.status })),
    [sorted],
  );

  const allDirs = useMemo(() => {
    const dirs = new Set<string>();
    for (const p of filePaths) {
      const parts = p.split("/");
      for (let i = 1; i < parts.length; i++)
        dirs.add(parts.slice(0, i).join("/"));
    }
    return [...dirs];
  }, [filePaths]);
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

  const options = useMemo(
    () => ({
      diffStyle: "unified" as const,
      themeType: theme,
      overflow: "wrap" as const,
    }),
    [theme],
  );

  if (!files || files.length === 0) {
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
          expandedItems={expandedItems ?? allDirs}
          onExpandedItemsChange={setExpandedItems}
          onSelectedItemsChange={handleSelect}
        />
      </div>
      <div className={css.diffPane} ref={paneRef}>
        {sorted.map((f) => (
          <div className={css.diffPatch} key={f.path} ref={setFileRef(f.path)}>
            <FileDiffEntry
              path={f.path}
              oldContents={f.oldContents}
              newContents={f.newContents}
              options={options}
            />
          </div>
        ))}
      </div>
    </div>
  );
});
