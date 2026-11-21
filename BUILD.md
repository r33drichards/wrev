# Build and deploy wrev on Wasmer

The WASIX build packages the `wrev` binary with embedded QuickJS-NG. Use
Wasmer 7.5.0, cargo-wasix 0.1.34, and WASI SDK 34. Install cargo-wasix and
run `cargo wasix download-toolchain v2026-09-18.1+rust-1.97` before building.
On Linux x86-64, make the toolchain linker executable with
`chmod +x "$(rustc +wasix --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld/ld.lld"`.

```sh
export CC_wasm32_wasmer_wasi=/path/to/wasi-sdk/bin/clang
python3 scripts/build_wasix.py --features mysql-backend
bash tests/wasmer_smoke.sh
```

The build script verifies the pinned Octocrab, rmcp, and rquickjs-sys source archives,
applies the WASIX compatibility changes in an ignored build directory, and
produces `dist/wrev.wasm`. It uses `wasix/Cargo.lock`. After changing the
dependency graph, run the build with `--update-lock` and commit the updated
lockfile after the build passes.

## GitHub Actions deployment

The [Wasmer workflow](.github/workflows/wasmer.yml) runs on manual dispatch
from `main`. The build job validates the package and uploads it. The deployment
job uses the GitHub `production` environment and reads `WASMER_TOKEN` from its
secrets. It deploys the artifact to `r33drichards/wrev` with a commit-specific
package version. It checks the deployment URL for a healthy listener and
verifies that an unsigned webhook receives HTTP 401.

Configure `WREV_GITHUB_TOKEN`, `WREV_WEBHOOK_SECRET`, `WREV_REPOSITORIES`, and
`OPENAI_MODEL` on the Wasmer app before verifying a deployment. See
[the listener setup](README.md#listen-for-the-label) and
[hosted credential setup](README.md#hosted-chatgpt-credentials) for the webhook
and MySQL configuration. Apply the hosted credential migration separately
before enabling the credential endpoint. The workflow uses only the Wasmer
deployment token.
