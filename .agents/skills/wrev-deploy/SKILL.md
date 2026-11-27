---
name: wrev-deploy
description: Deploy, verify, or roll back the Wrev production reviewer on Railway, including releases that change the pinned r33drichards/wrev binary and the r33drichards/webhook worker. Use for authorized production rollouts, PR-label and comment smoke tests, and diagnosis of those rollouts.
---

# Deploy Wrev on Railway

Wrev's native Rust reviewer lives in `r33drichards/wrev` (`main`). The Go worker, deployment Dockerfile, webhook handler, and volume-backed state live in `r33drichards/webhook` (`master`). Typical local clones are `~/wrev-reviewbench` and `~/wrev-railway-worker`. Confirm each remote and checkout before publishing. The worker is a fork. Use `gh -R r33drichards/webhook` for PR operations and `--head r33drichards:<branch>` when opening a worker PR. Use `$wrev-develop` for feature implementation and local validation.

Target the existing Railway service `wasm-webhook / production / wrev-review`. Verify the project and service against the linked deployment before mutating it.

- [Production service](https://railway.com/project/e8148bbc-ca01-44a6-b7e5-bfeddece00c7/service/c3eec083-9cb9-4de1-98d5-d03547b1355f)
- [Worker deployment runbook](https://github.com/r33drichards/webhook/blob/master/docs/Railway-wrev.md)
- [Production health endpoint](https://wrev-review-production.up.railway.app/)

## Release

1. Check the intended merged commit in each affected repository. For a Rust change, merge it first. Wrev's serialized Actions workflow proposes the latest merged SHA in the worker Dockerfile. Verify and merge that worker PR to release it. For worker-only changes, retain the verified pin. Keep the Railway `WREV_REVISION` override unset so the committed Dockerfile pin is authoritative.
2. Check the live deployment ID, health, and authenticated `GET /admin/reviews` queue status. Preserve the existing Dockerfile builder, one replica, `/` health check, and `/data` mount. The health check is configured directly in Railway. Preserve the queue, encrypted grant and its encryption key, and per-session checkpoints. For incompatible reviewer prompts, tools, or checkpoint formats, drain queued and running jobs before cutover and isolate the new format. The Wasmtime-to-native cutover used separate checkpoint directories.
3. Verify credential configuration by name and purpose. `WREV_GITHUB_TOKEN` reads repo content, `WREV_GITHUB_COMMENT_TOKEN` can write PR comments and remove the configured trigger label, and `OPENAI_MODEL` selects the model. Preserve the existing encrypted ChatGPT grant. Handle tokens through authenticated tools or stdin with approval for the intended credential and destination. Filter Railway variable output to non-secret values or presence checks.
4. Prefer the connected Railway tools for service status and logs. Railway follows `webhook/master`. If the merge does not trigger a deployment of the intended commit, deploy a clean checkout at the exact merged worker SHA. The linked CLI flow used `npx --yes @railway/cli up --detach --message '<worker revision and purpose>'`.
5. Wait for the specific deployment to succeed. Check build logs for the pinned native reviewer build, confirm service shape and volume mount, and check HTTP 200 on `/`. If a release breaks production, diagnose the specific deployment, then use the last known-good worker deployment and its matching reviewer pin for an authorized rollback. Check compatibility with the current volume state first. Keep the existing volume attached.

## Smoke test

Use an approved owned pilot PR at a known head, such as [Wrev PR #9](https://github.com/r33drichards/wrev/pull/9). Inspect its webhook and label before the test. If enabling a disabled hook is authorized and necessary, restore its previous configuration afterward. If the PR already carries the trigger label, clear the old label before applying it to create a new delivery.

- Apply the configured label, normally `wrev`. Check the signed GitHub delivery and watch the new session in `/admin/reviews` for the expected repository, PR, and head SHA.
- Verify the start comment and its update to the terminal status on the same comment ID. For success, compare the posted findings with the stored result and verify GitHub removed the trigger label automatically. A successful deployment or HTTP 200 alone does not verify model inference.
- If cancellation changed, exercise it on a separate attempt. Remove the label while the job is running and check that it stays canceled. Reapply to verify a new session ID.
- Correlate incomplete runs with worker and provider logs. A provider response of `subscription_sharing_usage_limit_exceeded` blocks inference. Report the failed session and current limit instead of repeatedly issuing new reviews. A later authorized retry can establish recovery.

Report affected PR links, merged worker and pinned reviewer SHAs, Railway deployment status, and the pilot PR and comment links. Include the verified terminal session state and label state.
