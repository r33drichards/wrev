# wrev

wrev reviews GitHub pull requests labeled `wrev`. It runs the Minimal MCP Agent
loop and embeds QuickJS. JavaScript gets three PR-scoped Rust tools backed by
Octocrab. Use `pulls_get` for PR metadata, `pulls_list_files` for changed-file pages,
and `repos_get_content` for Contents API data at the pinned base or head.
The tools preserve GitHub response fields and leave pagination and decoding
to the caller. The runner writes a JSON review to stdout. The Railway worker stores that result with the review session.

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

Native `wrev review` requires Git. It fetches the PR's base and head commits
into a temporary repository and uses the same read-only tools as
`wrev reviewbench CHECKOUT PR_JSON`. It reads Git objects without running code
from the pull request. Set `WREV_SESSION_DIR` and build with
`--features sqlite-backend` to persist checkpoints.

## ReviewBench results

Wrev and the Codex CLI each reviewed the same 219 frozen pull requests from the
[ReviewBench full corpus](https://github.com/review-bench/ReviewBench/blob/082f65f6746c39b6ac25b0a62c1b0d5131a3803a/corpus/manifest.json)
with GPT-6 Luna (`gpt-6-luna`). The
[ReviewBench offline judge](https://github.com/review-bench/ReviewBench/blob/082f65f6746c39b6ac25b0a62c1b0d5131a3803a/docs/OFFLINE_EVAL.md)
used GPT-6.1 Terra (`gpt-6.1-terra`) to match and classify findings.
These are micro-averaged results from one local pass per reviewer using a
development version of Wrev's local benchmark adapter. Both reviewers
completed all 219 pull requests.

| Metric | Wrev | Codex CLI |
| --- | ---: | ---: |
| Grounded precision | 79.7% | **83.2%** |
| Grounded recall | 2.6% | **4.7%** |
| Augmented precision | 62.5% | **70.8%** |
| Augmented recall | 3.4% | **6.1%** |
| Findings submitted | 128 | **209** |
| False positives (% of submitted findings) | 48 (37.5%) | **61 (29.2%)** |
| Novel true positives | 21 | **39** |

## Wasmer webhook

Wasmer uses WCGI to start a fresh `wrev wcgi` process for each HTTP request.
Configure GitHub's Pull requests webhook to send JSON to `/github/webhook`,
using the same `WREV_WEBHOOK_SECRET` as the app. Set `WREV_REPOSITORIES` to the
comma-separated repository allowlist. A signed, matching label event runs its
review inline and returns HTTP 204 on success. Reviews go to stderr as JSON,
which Wasmer captures in app logs. Keep logs private when reviewing private code.

GitHub expects a webhook response within 10 seconds. A longer review can appear
as a timed-out delivery, and a redelivery can run the review again. WCGI has no
cross-invocation queue or deduplication. `GET /health` checks webhook configuration.

## Run reviews in Wasmtime

Build the WASI Preview 2 command with `cargo build --locked --release --target
wasm32-wasip2 --bin wrev` and run it with Wasmtime 51. It supports `review`
and `models`, with HTTPS requests provided by the host through WASI HTTP.
Supply credentials through the host environment. The Wasmer WCGI deployment
can still receive webhooks directly. To use the Wasmtime component from a
webhook server, configure that server to invoke `review OWNER/REPO PR_NUMBER`.
See [build and run instructions](BUILD.md#wasmtime).

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
| `WREV_LABEL` | `wrev` | PR label that triggers a review |
| `WREV_WEBHOOK_BODY_BYTES` | 1048576 | Maximum webhook request body |
| `WREV_AUTH_TIMEOUT_SECS` | 30 | One sign-in or credential refresh HTTP request |
| `WREV_OAUTH_CALLBACK_TIMEOUT_SECS` | 300 | Time to complete the local browser sign-in |
| `WREV_OAUTH_READ_TIMEOUT_SECS` | 2 | Reading the local sign-in callback |

The WCGI webhook also requires `WREV_REPOSITORIES` and `WREV_WEBHOOK_SECRET`.
Both review modes require `OPENAI_MODEL` and `WREV_GITHUB_TOKEN`. Supply
`CHATGPT_ACCESS_TOKEN` for noninteractive deployments.

### Hosted ChatGPT credentials

Build with `mysql-backend`, apply
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

When enabled, the webhook handler reads this record for each model call. It refreshes
near expiry and stores the rotated credentials under a MySQL lock shared across
replicas. The hosted credential store takes precedence over the environment
token for webhook reviews. If a refresh fails or is interrupted, provision a
fresh grant. Use a dedicated OAuth grant for the deployment so another client
doesn't independently rotate the same refresh token. Confirm that the deployment
is eligible for [ChatGPT plan usage](https://developers.openai.com/siwc/token-sharing-open-source).
