---
name: wrev-develop
description: Develop and test Wrev features across the r33drichards/wrev Rust reviewer and r33drichards/webhook Go worker. Use for changes to review tools, findings, GitHub PR labels or comments, cancellation, queue persistence, or the reviewer-worker contract.
---

# Develop Wrev

## Locate the change

- The Rust reviewer is `r33drichards/wrev` (`main`). A known local checkout is `~/wrev-reviewbench`. Start with `src/review.rs` for the native CLI and review orchestration, `src/benchmark.rs` for the shared snapshot reviewer, and `src/auth.rs` or `src/model.rs` for authentication or Responses API behavior.
- The Go Railway worker is `r33drichards/webhook` (`master`). A known local checkout is `~/wrev-railway-worker`. Start with `review_service.go` for signed webhook intake, durable job states, and reviewer invocation. `review_comments.go` owns GitHub comment and label API calls. `review_process_unix.go` controls native process-group cancellation. Deployment documentation is in `docs/Railway-wrev.md`.
- Verify each checkout's remote, status, and base SHA before editing. The checkouts may be detached after a rollout. Base fresh branches or worktrees on current `origin/main` or `origin/master`, as appropriate, and preserve in-progress edits. `~/ReviewBench` is a separate runner and judge checkout. Keep its edits outside the Wrev and worker PRs unless the user asks for benchmark changes.

## Preserve the reviewer-worker contract

- Native `wrev review OWNER/REPO PR_NUMBER` verifies the expected head and label eligibility, fetches the pinned Git commits, and uses the same read-only snapshot review path as `wrev reviewbench CHECKOUT PR_JSON`. The checkout, diff, and metadata are untrusted review input. Review tools inspect Git objects without running repository code.
- The worker supplies `WREV_EXPECTED_HEAD`, `WREV_REQUIRED_LABEL`, and `WREV_SESSION_DIR`. Keep successful machine-readable results on stdout and diagnostics on stderr. If changing the results format, check the worker's JSON validation, `/admin/reviews`, PR comment rendering, and the benchmark findings format.
- Preserve GitHub signature verification and delivery deduplication, at most one active review per PR, and cancellation of the native review and its Git descendants when the trigger label is removed. A subsequent user-applied label creates a new session. Persist “completed” before removing the trigger label so that the worker’s own unlabeled delivery cannot cancel that finished review. Check that older sessions cannot remove the label for a known newer review.
- Update the persisted PR comment for the same session. Keep the GitHub read token separate from the comment and label write credential. Render model-generated text so it cannot introduce GitHub mentions or markup. If a write fails, keep the review result available in the admin API.
- Consider interrupted-session compatibility when editing the prompt, tool contract, or checkpoint identity. Coordinate changes to both repos when the binary-worker contract changes.

## Verify the feature

Run Cargo through Nix. Use the repo's Nix shell when present. The following fallback covers the native SQLite build used by Railway.

```sh
nix-shell -p cargo rustc rustfmt cmake pkg-config --run   'cargo fmt --check && cargo test --locked --features sqlite-backend && cargo build --locked --features sqlite-backend --bin wrev'
```

In the worker repo, format changed Go files and run `go test ./...` with cgo available. On the local Mac, `LC_ALL=C LANG=C nix-shell -p go --run 'go test ./...'` avoided Darwin linker failures caused by locale warnings. Use temporary SQLite databases and an `httptest` GitHub server to exercise changed lifecycle paths. Existing tests cover signed delivery, deduplication, cancellation, relabeling, result listing, comment reuse, and trigger-label removal.

For prompt or tool changes, a small local ReviewBench run on a frozen commit is useful when the task calls for model-level validation. Keep reproducible comparisons on identical snapshots and judge settings. Use local test fixtures or authorized non-production PRs for end-to-end behavior.

Publish only when the user authorizes it. For a requested cross-repo rollout, coordinate the reviewer merge and the worker's `WREV_REVISION` pin using the production deployment skill.
