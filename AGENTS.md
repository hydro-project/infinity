# Agent Guidelines

## Commit Messages

Use [Conventional Commits](https://www.conventionalcommits.org/) format:

```
<type>[optional scope]: <short summary>

<markdown body>
```

Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `ci`, `perf`.

For breaking changes, append `!` after the type (e.g. `refactor(infinity-daemon)!:`), and add `BREAKING CHANGE: ...` footers
describing each breaking change (or summarizing if many).

## Descriptions

Write the description in **Markdown**. Use bullet lists, code spans, and headings to organize multi-topic changes. Example:

```
feat(infinity-daemon): add session persistence via `sessions.json`

* Store sessions to `~/.infinity/sessions.json` on shutdown
* Restore on startup via `SessionStore::load()`
* Skip sessions older than 7 days
```

## Development

* Run `./check.bash` to test your changes before finalizing.
* Never ignore errors (`Result::Err`), make sure they are passed along or at least logged.
* Use `.expect("...")` with a descriptive message instead of `.unwrap()`. Prefix the message with "bug:" if a panic
  would represent a logical bug in our code (rather than a runtime error out of our control).
