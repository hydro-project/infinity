---
sidebar_position: 3
title: Coding with Git
---

# Coding with Git

For plain git repos (no `.jj` directory), Infinity Code uses [git worktrees](https://git-scm.com/docs/git-worktree) to isolate agent changes. Each agent thread gets its own worktree on a branch named `sandbox-{thread_id}`. No extra dependencies beyond git. Your working directory is never modified; changes live on sandbox branches that you inspect and merge when ready.

## Walkthrough

Start the agent in your repo:

```bash
cd ~/my-project
infinity
```

Ask it to create a file:

```
> Create a hello.py that prints hello world
```

The agent creates the file in its sandbox worktree. Back in your terminal, you can see the new branch:

```
$ git branch
* main
  sandbox-a1b2c3d4
```

Check what the agent wrote:

```
$ git diff main...sandbox-a1b2c3d4
diff --git a/hello.py b/hello.py
new file mode 100644
--- /dev/null
+++ b/hello.py
@@ -0,0 +1,2 @@
+if __name__ == "__main__":
+    print("hello world")
```

Now say you want to step away. Press Ctrl+D. Since the agent is idle, the CLI will immediately shut down. If it were busy, you'd get a picker to choose "Continue running agent in background" or "Shut down agent". Either way, your sandbox is persistent and you can resume your work later.

Later, come back and pick up where you left off:

```bash
infinity
```

Press `/load` (or Ctrl+L) to open the session picker and select your previous session. Ask for another change:

```
> Add a CLI argument so it greets by name
```

The agent updates the file on the same sandbox branch. Check the full diff:

```
$ git diff main...sandbox-a1b2c3d4
diff --git a/hello.py b/hello.py
new file mode 100644
--- /dev/null
+++ b/hello.py
@@ -0,0 +1,7 @@
+import sys
+
+if __name__ == "__main__":
+    if len(sys.argv) > 1:
+        print(f"hello {sys.argv[1]}")
+    else:
+        print("hello world")
```

When you're happy with the changes, merge them into your branch:

```bash
git merge sandbox-a1b2c3d4
```

Or cherry-pick, rebase, whatever fits your workflow.

## Child threads

When the agent needs to do parallel work, it spawns child threads. Each child gets its own sandbox branch. Ask the agent:

```
> Add unit tests and update the README
```

It spawns two child threads, one for tests and one for the README. You'll see multiple sandbox branches:

```
$ git branch
* main
  sandbox-a1b2c3d4
  sandbox-e5f6g7h8
  sandbox-i9j0k1l2
```

The agent automatically squashes child sandbox branches into the parent when they finish. So `sandbox-a1b2c3d4` (the root) ends up with all the changes, and you only need to merge that one:

```bash
git merge sandbox-a1b2c3d4
```

## Tip: consider adding Jujutsu

You can add jj on top of any git repo without changing your git workflow:

```bash
jj git init --colocate
```

This gives you access to the [Jujutsu workflow](./coding-with-jj.md), which is generally smoother for inspecting and incorporating agent changes. Your existing git history and remotes are unaffected.
