import { useMemo, useCallback, useRef, useState, memo } from "react";
import {
  CodeView,
  type CodeViewHandle,
  WorkerPoolContextProvider,
} from "@pierre/diffs/react";
import { parseDiffFromFile, type CodeViewItem } from "@pierre/diffs";
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
  workerFactory?: () => Worker;
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

export const DiffView = memo(function DiffView({
  files,
  theme,
  workerFactory,
}: DiffViewProps) {
  const viewerRef = useRef<CodeViewHandle<undefined>>(null);

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

  const diffCacheRef = useRef<
    Map<
      string,
      {
        old: string;
        new: string;
        fileDiff: ReturnType<typeof parseDiffFromFile>;
        version: number;
      }
    >
  >(new Map());

  const items: CodeViewItem[] = useMemo(() => {
    const cache = diffCacheRef.current;
    return sorted.map((f) => {
      const cached = cache.get(f.path);
      if (
        cached &&
        cached.old === f.oldContents &&
        cached.new === f.newContents
      ) {
        return {
          type: "diff" as const,
          id: `diff:${f.path}`,
          fileDiff: cached.fileDiff,
          version: cached.version,
        };
      }
      const version = cached ? cached.version + 1 : 0;
      const fileDiff = parseDiffFromFile(
        {
          name: f.path,
          contents: f.oldContents,
          cacheKey: `${f.path}:old:${version}`,
        },
        {
          name: f.path,
          contents: f.newContents,
          cacheKey: `${f.path}:new:${version}`,
        },
      );
      cache.set(f.path, {
        old: f.oldContents,
        new: f.newContents,
        fileDiff,
        version,
      });
      return { type: "diff" as const, id: `diff:${f.path}`, fileDiff, version };
    });
  }, [sorted]);

  const handleSelect = useCallback((paths: string[]) => {
    setSelectedItems(paths);
    if (paths[0]) {
      viewerRef.current?.scrollTo({
        type: "item",
        id: `diff:${paths[0]}`,
        align: "start",
        behavior: "smooth-auto",
      });
    }
  }, []);

  const options = useMemo(
    () => ({
      theme: { dark: "pierre-dark", light: "pierre-light" } as const,
      themeType: theme,
      diffStyle: "unified" as const,
      overflow: "wrap" as const,
      stickyHeaders: true,
      layout: { paddingTop: 16, paddingBottom: 16, gap: 16 },
    }),
    [theme],
  );

  if (!files || files.length === 0) {
    return <div className={css.empty}>No changes yet</div>;
  }

  const diffContent = (
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
      <div className={css.diffPaneWrapper}>
        <CodeView
          ref={viewerRef}
          items={items}
          className={css.diffPane}
          options={options}
        />
      </div>
    </div>
  );

  if (workerFactory) {
    return (
      <WorkerPoolContextProvider
        poolOptions={{ workerFactory }}
        highlighterOptions={{
          theme: { dark: "pierre-dark", light: "pierre-light" },
          langs: ["typescript", "javascript", "css", "html", "json", "rust"],
        }}
      >
        {diffContent}
      </WorkerPoolContextProvider>
    );
  }

  return diffContent;
});
