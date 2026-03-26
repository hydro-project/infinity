---
sidebar_position: 2
title: Coding with Jujutsu
---

# Coding with Jujutsu

Jujutsu is the recommended way to use Infinity Code. When your repo has a `.jj` directory, the sandbox creates isolated [Jujutsu workspaces](https://jj-vcs.dev) via `jj workspace add`. Each agent thread gets its own workspace with a bookmark named `sandbox-{thread_id}`. Your working copy is never touched — you inspect the agent's changes and squash them in when you're ready.

Don't have jj yet? You can add it on top of any git repo without affecting your existing workflow:

```bash
jj git init --colocate
```

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

The agent creates the file in its sandbox. In another terminal, you can see what it did:

```
$ jj log -r 'bookmarks("sandbox-*")'
○  ksqvpntq sandbox-a1b2c3d4 2025-03-26 12:01
│  Create hello.py
~
```

```
$ jj show sandbox-a1b2c3d4
Added regular file hello.py:
    1: print("hello world")
```

Now say you want to step away. Press Ctrl+D. Since the agent is idle, the CLI will immdiately shut down. If it were busy, you'd get a picker to choose "Continue running agent in background" or "Shut down agent". Either way, your sandbox is persistent and you can resume your work later.

Come back later, re-launch, and load your session:

```bash
infinity
# press /load (or Ctrl+L), pick your session
```

Ask for another change:

```
> Add a CLI argument so it greets by name
```

The agent edits the file in the same sandbox. The bookmark moves forward:

```
$ jj log -r 'bookmarks("sandbox-*")'
○  mwzrtpkl sandbox-a1b2c3d4 2025-03-26 12:05
│  Add CLI argument for name greeting
~
```

Check the full diff against main:

```
$ jj diff --from main --to sandbox-a1b2c3d4
Added regular file hello.py:
    1: import sys
    2:
    3: name = sys.argv[1] if len(sys.argv) > 1 else "world"
    4: print(f"hello {name}")
```

When you're happy, pull the changes into your working copy:

```bash
jj squash --from sandbox-a1b2c3d4
```

That's it — the agent's changes are now in your current change. You can also cherry-pick, rebase, or use any other jj operation you prefer.

## Child threads

When the agent needs to do multiple things at once, it spawns child threads. Each child gets its own jj workspace and bookmark.

For example, ask:

```
> Add unit tests and update the README
```

The agent spawns two child threads that work in parallel. While they're running, the jj log shows separate bookmarks for each:

```
$ jj log -r 'bookmarks("sandbox-*")'
○  xpqrstvw sandbox-e5f6g7h8 2025-03-26 12:10
│  Add unit tests for hello.py
│ ○  yznmlkjh sandbox-i9j0k1l2 2025-03-26 12:10
├─╯  Update README with usage instructions
○  mwzrtpkl sandbox-a1b2c3d4 2025-03-26 12:10
│  Add CLI argument for name greeting
~
```

As each child finishes, the agent squashes its changes into the parent sandbox automatically via the `squash_sandbox` tool. You only ever need to squash the root:

```bash
jj squash --from sandbox-a1b2c3d4
```
