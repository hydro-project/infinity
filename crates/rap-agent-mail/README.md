# rap-agent-mail

A [RAP](https://infinity.hydro.run/docs/rap/what-is-rap) tool server that lets Infinity agents message agents in other Infinity daemons, on the same machine or on remote hosts reached over SSH.

Each daemon runs one mailbox (a `lifetime: daemon` RAP server). Agents get five tools:

| Tool | What it does |
|---|---|
| `open_inbox` | Subscription: makes the calling thread reachable as `host/name`. Each message arrives as an event of this call. |
| `list_agents` | Lists open inboxes on this host and on every connected peer host. |
| `send_message` | One-way message; returns once the recipient's host has delivered it, or reports it queued. |
| `ask_agent` | Question; the tool call stays pending until the recipient calls `reply`. |
| `reply` | Answers a received question; the answer becomes the asker's tool result. |

Mailboxes link to the peers in `~/.infinity/remotes.json` by forwarding a Unix socket over SSH, so a host that can only be dialed (not dial back) can still send and receive. Messages wait in a durable outbox until the peer acknowledges them.

See the [Agent Mail docs](https://infinity.hydro.run/docs/infinity-code/agent-mail) for setup and semantics.
