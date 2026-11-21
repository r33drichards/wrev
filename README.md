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

## Deployment configuration

`wrev` loads deployment settings with the Rust `config` crate. Set each option
independently in the process environment. Unset options use
the defaults below. Timeouts and byte limits must be positive integers.
Supply credentials through your deployment's secret store. See the
[Twelve-Factor config guidance](https://12factor.net/config).

| Variable | Default | Scope |
| --- | ---: | --- |
| `WREV_MODEL_TIMEOUT_SECS` | 180 | One model or model-catalog HTTP request |
| `WREV_GITHUB_TIMEOUT_SECS` | 20 | One GitHub API request |
| `WREV_SCRIPT_TIMEOUT_SECS` | 120 | One QuickJS invocation, including tool calls |
| `WREV_SCRIPT_CODE_BYTES` | 65536 | JavaScript source per invocation |
| `WREV_SCRIPT_OUTPUT_BYTES` | 65536 | Console output per invocation |
| `WREV_SCRIPT_MEMORY_BYTES` | 33554432 | QuickJS heap limit |
| `WREV_SCRIPT_STACK_BYTES` | 262144 | QuickJS stack limit |
| `WREV_BIND_ADDR` | `0.0.0.0` | Listener IP address |
| `PORT` | 8080 | Listener port |
| `WREV_LABEL` | `wrev` | PR label that triggers a review |
| `WREV_QUEUE_CAPACITY` | 16 | Queued webhook reviews |
| `WREV_DEDUP_CAPACITY` | 4096 | Recently completed reviews kept for deduplication |
| `WREV_WEBHOOK_BODY_BYTES` | 1048576 | Maximum webhook request body |
| `WREV_AUTH_TIMEOUT_SECS` | 30 | One sign-in or credential refresh HTTP request |
| `WREV_OAUTH_CALLBACK_TIMEOUT_SECS` | 300 | Time to complete the local browser sign-in |
| `WREV_OAUTH_READ_TIMEOUT_SECS` | 2 | Reading the local sign-in callback |

The listener also requires `WREV_REPOSITORIES` and `WREV_WEBHOOK_SECRET`.
Both review modes require `OPENAI_MODEL` and `WREV_GITHUB_TOKEN`. Supply
`CHATGPT_ACCESS_TOKEN` for noninteractive deployments.

### Hosted ChatGPT credentials

Build the listener with `mysql-backend`, apply
`migrations/002_chatgpt_credentials.sql` to its MySQL database, and configure
the [verified TLS connection](MYSQL_BACKEND.md). Set two independent runtime
secrets: `WREV_CREDENTIAL_ADMIN_TOKEN` (at least 32 random ASCII characters)
and `WREV_CREDENTIAL_ENCRYPTION_KEY` (32 random bytes encoded as 64 hex
characters). Use the same encryption key on every replica and keep it available
across deployments. Leave the admin token unset to keep the endpoint disabled.

Send an HTTPS `PUT /admin/chatgpt-credentials` with
`Authorization: Bearer <WREV_CREDENTIAL_ADMIN_TOKEN>` and a JSON body containing
`access_token` and `expires_at` (Unix seconds). Include both `refresh_token`
and the issued OAuth `client_id` to enable refresh. Supply the body from private
input or a secret manager. HTTP 204 means MySQL accepted the encrypted replacement.
The endpoint has no read method.

When enabled, the listener reads this record for each model call. It refreshes
near expiry and stores the rotated credentials under a MySQL lock shared across
replicas. The hosted credential store takes precedence over the environment
token for webhook reviews. If a refresh fails or is interrupted, provision a
fresh grant. Use a dedicated OAuth grant for the deployment so another client
doesn't independently rotate the same refresh token. Confirm that the deployment
is eligible for [ChatGPT plan usage](https://developers.openai.com/siwc/token-sharing-open-source).
