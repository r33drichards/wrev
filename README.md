# wrev

wrev reviews GitHub pull requests labeled `wrev`. It runs the Minimal MCP Agent
loop and embeds QuickJS. JavaScript gets three PR-scoped Rust tools backed by
Octocrab. Use `pulls_get` for PR metadata, `pulls_list_files` for changed-file pages,
and `repos_get_content` for Contents API data at the pinned base or head.
The tools preserve GitHub response fields and leave pagination and decoding
to the caller. The runner writes a JSON review to stdout.

## Run

Build on native Linux or macOS with `cargo build --locked`. Use
`cargo run --locked -- login` to sign in with ChatGPT and
`cargo run --locked -- models` to find your account's model slug.

Set `OPENAI_MODEL` and supply `WREV_GITHUB_TOKEN` through your secret store.
Use a fine-grained GitHub token restricted to the repositories you review,
with read access to Contents and Pull requests. A fork's head files are
available only when the credential can read the fork. For noninteractive
hosting, supply the template's Sign in with ChatGPT access token through
`CHATGPT_ACCESS_TOKEN`.

```sh
cargo run --locked -- review OWNER/REPO 123
```

## Listen for the label

Set `WREV_WEBHOOK_SECRET` and `WREV_REPOSITORIES` (a comma-separated
allowlist of `OWNER/REPO` names). Start the listener:

```sh
cargo run --locked -- listen
```

The listener binds `0.0.0.0:$PORT` (8080 by default). Configure a GitHub
webhook for **Pull requests**, send JSON to `/github/webhook` through HTTPS,
and use the same signing secret. Create and apply the `wrev` label in each
repository you want reviewed. `GET /health` is the health endpoint.

The listener acknowledges queued reviews with HTTP 202 and processes them
one at a time. Deduplication uses repository, PR number, and head SHA. The
queue and deduplication state are in memory. Restarting the process clears
both. Reviews go to stdout as JSON, so keep the runner's logs private when
reviewing private code.
