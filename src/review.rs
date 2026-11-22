use crate::config::{self, ReviewConfig};
use crate::review_search::{self, File, Side};
#[cfg(all(target_os = "wasi", target_env = "p2"))]
use crate::wasi_github::GitHub;
use anyhow::{Context, Result, anyhow, ensure};
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
use hmac::{Hmac, Mac};
use minimal_mcp_agent::{agent, quickjs_tools::QuickJsTools};
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
use octocrab::{Octocrab as GitHub, Page};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
use sha2::Sha256;
#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
use std::{
    collections::HashSet,
    io::{self, Read, Write},
};
use std::{env, time::Duration};
#[cfg(all(test, not(all(target_os = "wasi", target_env = "p2"))))]
use tiny_http::{Method, Request, Response, Server};
use tokio::runtime::Runtime;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Target {
    repository: String,
    number: u64,
    head: String,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct Ref {
    sha: String,
    repo: Option<Repository>,
}

#[derive(Deserialize)]
struct Pull {
    state: String,
    base: Ref,
    head: Ref,
    labels: Vec<Label>,
    #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
    title: Option<String>,
    #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
    body: Option<String>,
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
#[derive(Deserialize)]
struct PullEvent {
    action: String,
    number: u64,
    label: Option<Label>,
    repository: Repository,
    pull_request: EventPull,
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
#[derive(Deserialize)]
struct EventPull {
    head: Ref,
}

pub(crate) fn allowed_repo(name: &str) -> bool {
    name.split_once('/').is_some_and(|(owner, repo)| {
        [owner, repo].iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
    })
}

pub(crate) fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn authenticate(runtime: &Runtime) -> Result<GitHub> {
    let _runtime = runtime.enter();
    let token = config::required("WREV_GITHUB_TOKEN")?;
    Ok(GitHub::builder().personal_token(token).build()?)
}

#[cfg(all(target_os = "wasi", target_env = "p2"))]
fn authenticate(_: &Runtime) -> Result<GitHub> {
    Ok(GitHub::new(config::required("WREV_GITHUB_TOKEN")?))
}

async fn pull(client: &GitHub, repository: &str, number: u64, timeout: Duration) -> Result<Pull> {
    ensure!(
        allowed_repo(repository) && number > 0,
        "Invalid pull request identity"
    );
    let route = format!("/repos/{repository}/pulls/{number}");
    github_get(client, route, None::<&()>, timeout)
        .await
        .context("Read pull request")
}

fn register_tools(
    client: &GitHub,
    target: &Target,
    pull: &Pull,
    settings: &ReviewConfig,
) -> Result<QuickJsTools> {
    ensure!(
        valid_sha(&pull.base.sha) && valid_sha(&pull.head.sha),
        "Invalid PR commit SHA"
    );
    let head_repo = pull.head.repo.as_ref().map(|repo| repo.full_name.clone());
    if let Some(repo) = &head_repo {
        ensure!(allowed_repo(repo), "Invalid head repository");
    }

    let mut tools = QuickJsTools::with_limits(settings.script);
    let timeout = settings.github_timeout;
    let pull_route = format!("/repos/{}/pulls/{}", target.repository, target.number);
    let files_route = format!("{pull_route}/files");
    let pull_client = client.clone();
    tools.register_async_rust(
        "pulls_get",
        "Get the pull request authorized for this review. Returns GitHub's JSON.",
        json!({"type":"object","additionalProperties":false}),
        move |_| {
            let client = pull_client.clone();
            let route = pull_route.clone();
            async move { github_get(&client, route, None::<&()>, timeout).await }
        },
    )?;

    let files_client = client.clone();
    tools.register_async_rust(
        "pulls_list_files",
        "List files for the authorized pull request. Returns an Octocrab-compatible page with items and pagination links.",
        json!({"type":"object","properties":{
            "page":{"type":"integer","minimum":1},
            "per_page":{"type":"integer","minimum":1,"maximum":100}
        },"additionalProperties":false}),
        move |args| {
            let client = files_client.clone();
            let route = files_route.clone();
            async move {
                let mut query = Vec::new();
                for (key, max) in [("page", u64::from(u32::MAX)), ("per_page", 100)] {
                    if let Some(value) = args.get(key) {
                        let value = value.as_u64().filter(|n| (1..=max).contains(n))
                            .with_context(|| format!("Invalid {key}"))?;
                        query.push((key, value));
                    }
                }
                github_files(&client, route, &query, timeout).await
            }
        },
    )?;

    let file_client = client.clone();
    let base = (
        target.repository.to_ascii_lowercase(),
        pull.base.sha.clone(),
    );
    let head = head_repo.map(|repo| (repo.to_ascii_lowercase(), pull.head.sha.clone()));
    let (search_base, search_head) = (base.clone(), head.clone());
    tools.register_async_rust(
        "repos_get_content",
        "Get GitHub Contents API JSON for a path at one of this PR's pinned repository and commit pairs.",
        json!({"type":"object","properties":{
            "repository":{"type":"string"}, "ref":{"type":"string"}, "path":{"type":"string"}
        },"required":["repository","ref","path"],"additionalProperties":false}),
        move |args| {
            let client = file_client.clone();
            let base = base.clone();
            let head = head.clone();
            async move {
                let repository = args.get("repository").and_then(Value::as_str)
                    .context("Missing repository")?.to_ascii_lowercase();
                let reference = args.get("ref").and_then(Value::as_str).context("Missing ref")?;
                let path = args.get("path").and_then(Value::as_str).context("Missing path")?;
                let pair = (repository, reference.to_owned());
                ensure!(pair == base || head.as_ref() == Some(&pair), "Repository and ref are outside this review");
                let route = file_route(&pair.0, path)?;
                github_get(&client, route, Some(&[("ref", reference)]), timeout).await
            }
        },
    )?;
    let search_client = client.clone();
    review_search::register(&mut tools, move |side, path| {
        let client = search_client.clone();
        let pair = match side {
            Side::Base => Some(search_base.clone()),
            Side::Head => search_head.clone(),
        };
        async move {
            let Some((repository, reference)) = pair else {
                return File::Skipped("head_unavailable");
            };
            let Ok(route) = file_route(&repository, &path) else {
                return File::Skipped("invalid_path");
            };
            let query = [("ref", reference.as_str())];
            let Ok(contents) = github_get::<Value>(&client, route, Some(&query), timeout).await
            else {
                return File::Skipped("unreadable");
            };
            decode_search_file(contents)
        }
    })?;
    Ok(tools)
}

fn decode_search_file(contents: Value) -> File {
    use base64::{Engine, engine::general_purpose::STANDARD};

    if contents["type"] != "file" {
        return File::Skipped("not_a_file");
    }
    let Some(size) = contents["size"].as_u64() else {
        return File::Skipped("unreadable");
    };
    if size > review_search::MAX_FILE_BYTES as u64 {
        return File::Skipped("too_large");
    }
    let Some(encoded) = contents["content"]
        .as_str()
        .filter(|_| contents["encoding"] == "base64")
    else {
        return File::Skipped("unsupported_encoding");
    };
    let max_encoded = review_search::MAX_FILE_BYTES.div_ceil(3) * 4;
    // Contents API base64 is line-wrapped. Limit the encoded payload too,
    // rather than trusting a claimed file size.
    let encoded: Vec<_> = encoded
        .bytes()
        .filter(|byte| !matches!(byte, b'\r' | b'\n'))
        .take(max_encoded + 1)
        .collect();
    if encoded.len() > max_encoded {
        return File::Skipped("too_large");
    }
    match STANDARD.decode(encoded) {
        Ok(bytes) if bytes.len() as u64 == size => File::Bytes(bytes),
        _ => File::Skipped("unreadable"),
    }
}

async fn github_get<T: serde::de::DeserializeOwned>(
    client: &GitHub,
    route: String,
    query: Option<&impl serde::Serialize>,
    timeout: Duration,
) -> Result<T> {
    #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
    return Ok(tokio::time::timeout(timeout, client.get(route, query)).await??);
    #[cfg(all(target_os = "wasi", target_env = "p2"))]
    client.get(route, query, timeout)
}

async fn github_files(
    client: &GitHub,
    route: String,
    query: &[(&str, u64)],
    timeout: Duration,
) -> Result<Value> {
    #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
    {
        let page: Page<Value> =
            tokio::time::timeout(timeout, client.get(route, Some(&query))).await??;
        Ok(serde_json::to_value(page)?)
    }
    #[cfg(all(target_os = "wasi", target_env = "p2"))]
    client.files(route, query, timeout)
}

fn file_route(repository: &str, path: &str) -> Result<String> {
    ensure!(
        path.len() <= 1024
            && path.split('/').all(|part| {
                !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', '\0'])
            }),
        "Invalid repository file path"
    );
    let mut url = url::Url::parse(&format!(
        "https://api.github.com/repos/{repository}/contents"
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("Invalid file URL"))?
        .extend(path.split('/'));
    Ok(url.path().to_owned())
}

fn run_review(
    runtime: &Runtime,
    client: &GitHub,
    settings: &ReviewConfig,
    model_client: &crate::model::Client,
    target: &Target,
    required_label: Option<&str>,
    access_token: impl Fn() -> Result<String>,
) -> Result<Value> {
    let pr = runtime.block_on(pull(
        client,
        &target.repository,
        target.number,
        settings.github_timeout,
    ))?;
    let eligible = |pr: &Pull| {
        pr.state == "open"
            && pr.head.sha == target.head
            && required_label
                .is_none_or(|required| pr.labels.iter().any(|label| label.name == required))
    };
    ensure!(
        eligible(&pr),
        "Pull request is closed, changed, or no longer labeled"
    );
    let tools = register_tools(client, target, &pr, settings)?;
    let input = include_str!("review_prompt.md");
    let identity = format!(
        "{}#{}:{}:{}",
        target.repository, target.number, target.head, pr.base.sha
    );
    let review = run_agent(
        runtime,
        settings,
        model_client,
        &tools,
        &identity,
        input,
        access_token,
    )?;
    let final_pr = runtime.block_on(pull(
        client,
        &target.repository,
        target.number,
        settings.github_timeout,
    ))?;
    ensure!(
        eligible(&final_pr) && final_pr.base.sha == pr.base.sha,
        "Pull request changed during review"
    );
    Ok(
        json!({"repository": target.repository, "number": target.number,
        "head": target.head, "review": review}),
    )
}

pub(crate) fn run_agent(
    runtime: &Runtime,
    settings: &ReviewConfig,
    model_client: &crate::model::Client,
    tools: &QuickJsTools,
    _identity: &str,
    input: &str,
    access_token: impl Fn() -> Result<String>,
) -> Result<String> {
    let definitions = QuickJsTools::definitions();
    let prompt = |messages: &[Value]| {
        crate::model::prompt(
            model_client,
            &access_token()?,
            &settings.model,
            messages,
            &definitions,
        )
    };
    let tool = |name: &str, args: &str| runtime.block_on(tools.call(name, args, false));
    let mut messages = Vec::new();
    #[cfg(feature = "sqlite-backend")]
    if let Some(dir) = env::var_os("WREV_SESSION_DIR") {
        use minimal_mcp_agent::durable::{Turn, run_durable_turn};
        use minimal_mcp_agent::durable_sqlite::SqliteBackend;
        let backend = SqliteBackend::new(std::path::PathBuf::from(dir));
        messages = run_durable_turn(
            &backend,
            Turn {
                session_id: _identity,
                request_id: "review",
                input,
                max_rounds: usize::MAX,
            },
            prompt,
            |name, args| {
                Ok(tool(name, args)
                    .unwrap_or_else(|err| json!({"isError":true,"error":err.to_string()})))
            },
            |_| {},
            &mut |_, _: &agent::Checkpoint| Ok(()),
        )?;
    } else {
        agent::run_turn(&mut messages, input, None, prompt, tool, |_| {})?;
    }
    #[cfg(not(feature = "sqlite-backend"))]
    agent::run_turn(&mut messages, input, None, prompt, tool, |_| {})?;
    messages
        .iter()
        .rev()
        .filter(|item| {
            item["type"] == "message"
                && item["role"] == "assistant"
                && item["phase"] != "commentary"
        })
        .find_map(|item| {
            let text = item["content"]
                .as_array()?
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        })
        .context("Model returned no final review")
}

pub(crate) fn benchmark_findings(
    repository: &str,
    number: u64,
    base: &str,
    head: &str,
    review: &str,
) -> Result<Value> {
    let review = review.trim();
    let review = review
        .strip_prefix("```json")
        .or_else(|| review.strip_prefix("```"))
        .and_then(|body| body.strip_suffix("```"))
        .unwrap_or(review);
    let result: Value =
        serde_json::from_str(review).context("Wrev returned invalid ReviewBench JSON")?;
    let findings = result["findings"]
        .as_array()
        .context("Wrev returned no findings array")?
        .iter()
        .map(|finding| {
            let mut finding = finding
                .as_object()
                .context("Invalid ReviewBench finding")?
                .clone();
            finding.insert("producer".into(), json!("wrev"));
            Ok(Value::Object(finding))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "pr": {
            "repo": format!("https://github.com/{repository}"),
            "pr_number": number,
            "base": base,
            "head": head
        },
        "agent": "wrev",
        "findings": findings
    }))
}

pub fn manual() -> Result<()> {
    let mut args = env::args().skip(2);
    let repository = args
        .next()
        .context("Usage: wrev review OWNER/REPO PR_NUMBER")?;
    let number = args.next().context("Missing PR number")?.parse()?;
    ensure!(args.next().is_none(), "Unexpected arguments");
    #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
    let runtime = Runtime::new()?;
    #[cfg(all(target_os = "wasi", target_env = "p2"))]
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let client = authenticate(&runtime)?;
    let settings = ReviewConfig::from_env()?;
    let current = runtime.block_on(pull(&client, &repository, number, settings.github_timeout))?;
    let expected_head = env::var("WREV_EXPECTED_HEAD").ok();
    if let Some(head) = &expected_head {
        ensure!(
            valid_sha(head) && current.head.sha == *head,
            "Pull request head changed before review"
        );
    }
    let required_label = env::var("WREV_REQUIRED_LABEL").ok();
    #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
    {
        let eligible = |pr: &Pull| {
            pr.state == "open"
                && required_label
                    .as_ref()
                    .is_none_or(|label| pr.labels.iter().any(|entry| &entry.name == label))
        };
        ensure!(
            eligible(&current),
            "Pull request is closed or no longer labeled"
        );
        let snapshot = crate::benchmark::Snapshot {
            nwo: repository,
            pr_number: number,
            base: current.base.sha,
            head: current.head.sha,
            title: current.title,
            body: current.body,
        };
        let checkout = crate::benchmark::fetch(&snapshot, &config::required("WREV_GITHUB_TOKEN")?)?;
        let mut result = crate::benchmark::review_snapshot(checkout.path(), &snapshot)?;
        let latest = runtime.block_on(pull(
            &client,
            &snapshot.nwo,
            number,
            settings.github_timeout,
        ))?;
        ensure!(
            eligible(&latest)
                && latest.base.sha == snapshot.base
                && latest.head.sha == snapshot.head,
            "Pull request changed during review"
        );
        result["repository"] = json!(snapshot.nwo);
        result["number"] = json!(number);
        result["head"] = json!(snapshot.head);
        result["review"] = json!(result["findings"].to_string());
        println!("{result}");
        Ok(())
    }
    #[cfg(all(target_os = "wasi", target_env = "p2"))]
    {
        let review = run_review(
            &runtime,
            &client,
            &settings,
            &crate::model::client()?,
            &Target {
                repository,
                number,
                head: current.head.sha,
            },
            required_label.as_deref(),
            config::chatgpt_token,
        )?;
        println!("{review}");
        Ok(())
    }
}

#[cfg(all(test, not(all(target_os = "wasi", target_env = "p2"))))]
pub(crate) fn header(request: &Request, name: &'static str) -> Option<String> {
    let mut matches = request
        .headers()
        .iter()
        .filter(|field| field.field.equiv(name));
    let first = matches.next()?.value.as_str().to_owned();
    matches.next().is_none().then_some(first)
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn signed(secret: &[u8], signature: &str, body: &[u8]) -> bool {
    if signature.len() != 71 {
        return false;
    }
    let Some(bytes) = signature
        .strip_prefix("sha256=")
        .and_then(|value| hex::decode(value).ok())
    else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&bytes).is_ok()
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn labeled_target(
    body: &[u8],
    repositories: &HashSet<String>,
    expected_label: &str,
) -> Result<Option<Target>> {
    let event: PullEvent = serde_json::from_slice(body)?;
    if event.action != "labeled"
        || !event
            .label
            .is_some_and(|label| label.name == expected_label)
        || !repositories.contains(&event.repository.full_name.to_ascii_lowercase())
    {
        return Ok(None);
    }
    ensure!(
        event.number > 0 && valid_sha(&event.pull_request.head.sha),
        "Invalid pull request"
    );
    Ok(Some(Target {
        repository: event.repository.full_name.to_ascii_lowercase(),
        number: event.number,
        head: event.pull_request.head.sha,
    }))
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn repositories() -> Result<HashSet<String>> {
    let repositories: HashSet<_> = config::required("WREV_REPOSITORIES")?
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    ensure!(
        !repositories.is_empty() && repositories.iter().all(|name| allowed_repo(name)),
        "Expected a comma-separated repository allowlist"
    );
    Ok(repositories)
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn webhook_target(
    body: impl Read,
    signature: Option<&str>,
    event: Option<&str>,
    secret: &[u8],
    repositories: &HashSet<String>,
    webhook: &config::WebhookConfig,
) -> std::result::Result<Target, u16> {
    let signature = signature.ok_or(401u16)?;
    let mut bytes = Vec::new();
    if body
        .take((webhook.body_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > webhook.body_bytes
    {
        return Err(413);
    }
    if !signed(secret, signature, &bytes) {
        return Err(401);
    }
    if event != Some("pull_request") {
        return Err(204);
    }
    labeled_target(&bytes, repositories, &webhook.label)
        .map_err(|_| 400u16)?
        .ok_or(204)
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
fn wcgi_request(method: &str, path: &str) -> Result<u16> {
    match (method, path) {
        ("GET", "/health") => {
            config::required("WREV_WEBHOOK_SECRET")?;
            config::required("WREV_GITHUB_TOKEN")?;
            repositories()?;
            config::WebhookConfig::from_env()?;
            ReviewConfig::from_env()?;
            Ok(200)
        }
        #[cfg(feature = "mysql-backend")]
        ("PUT", "/admin/chatgpt-credentials") => {
            let authorization = env::var("HTTP_AUTHORIZATION").ok();
            Ok(
                crate::hosted_auth::HostedAuth::from_env()?.map_or(404, |store| {
                    store.put_body(authorization.as_deref(), io::stdin().lock())
                }),
            )
        }
        ("POST", "/github/webhook") => {
            let Ok(signature) = env::var("HTTP_X_HUB_SIGNATURE_256") else {
                return Ok(401);
            };
            let secret = config::required("WREV_WEBHOOK_SECRET")?;
            let repositories = repositories()?;
            let webhook = config::WebhookConfig::from_env()?;
            let event = env::var("HTTP_X_GITHUB_EVENT").ok();
            let target = match webhook_target(
                io::stdin().lock(),
                Some(&signature),
                event.as_deref(),
                secret.as_bytes(),
                &repositories,
                &webhook,
            ) {
                Ok(target) => target,
                Err(status) => return Ok(status),
            };
            let runtime = Runtime::new()?;
            let client = authenticate(&runtime)?;
            #[cfg(feature = "mysql-backend")]
            let credentials = crate::hosted_auth::HostedAuth::from_env()?;
            let review = run_review(
                &runtime,
                &client,
                &ReviewConfig::from_env()?,
                &crate::model::client()?,
                &target,
                Some(&webhook.label),
                || {
                    #[cfg(feature = "mysql-backend")]
                    if let Some(store) = &credentials {
                        return store.access_token();
                    }
                    config::chatgpt_token()
                },
            )?;
            eprintln!("{review}");
            Ok(204)
        }
        _ => Ok(404),
    }
}

#[cfg(not(all(target_os = "wasi", target_env = "p2")))]
pub fn wcgi() -> Result<()> {
    let method = env::var("REQUEST_METHOD").unwrap_or_default();
    let uri = env::var("REQUEST_URI").unwrap_or_default();
    let path = env::var("PATH_INFO").ok().filter(|path| !path.is_empty());
    let path = path
        .as_deref()
        .unwrap_or_else(|| uri.split('?').next().unwrap_or_default());
    let status = wcgi_request(&method, path).unwrap_or_else(|_| {
        eprintln!("WCGI request failed for a configured route");
        500
    });
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Content Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    write!(
        io::stdout().lock(),
        "Status: {status} {reason}\r\nContent-Type: text/plain\r\n\r\n"
    )?;
    Ok(())
}

#[cfg(all(test, not(all(target_os = "wasi", target_env = "p2"))))]
mod tests {
    use super::*;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const BASE: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn verifies_label_repository_and_signature() -> Result<()> {
        let repo = HashSet::from(["acme/project".to_owned()]);
        let mut event = json!({
            "action": "labeled", "number": 7, "label": {"name":"wrev"},
            "repository":{"full_name":"Acme/Project"},
            "pull_request":{"head":{"sha": HEAD}}
        });
        let body = serde_json::to_vec(&event)?;
        let secret = b"test-webhook-secret";
        let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
        mac.update(&body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(signed(secret, &signature, &body));
        assert!(!signed(secret, &signature, b"tampered"));
        assert_eq!(labeled_target(&body, &repo, "wrev")?.unwrap().head, HEAD);
        assert!(labeled_target(&body, &HashSet::new(), "wrev")?.is_none());
        event["label"]["name"] = json!("wreview");
        assert!(labeled_target(&serde_json::to_vec(&event)?, &repo, "wrev")?.is_none());
        Ok(())
    }

    #[test]
    fn quickjs_reads_scoped_github_data_using_only_authenticated_gets() -> Result<()> {
        let fixture = Server::http("127.0.0.1:0").map_err(|error| anyhow!("{error}"))?;
        let endpoint = format!("http://{}", fixture.server_addr());
        let expected = [
            (
                "/repos/acme/project/pulls/7".to_owned(),
                json!({"title":"Review","custom":"kept"}),
            ),
            (
                "/repos/acme/project/pulls/7/files?page=1&per_page=10".to_owned(),
                json!([
                    {"filename":"src/new file.rs","status":"modified","patch":"+new text"}
                ]),
            ),
            (
                format!("/repos/contributor/fork/contents/src/new%20file.rs?ref={HEAD}"),
                json!({"type":"file","size":8,"encoding":"base64","content":"bmV3IHRleHQ="}),
            ),
            (
                format!("/repos/contributor/fork/contents/src/new%20file.rs?ref={HEAD}"),
                json!({"type":"file","size":8,"encoding":"base64","content":"bmV3\nIHRleHQ="}),
            ),
            (
                format!("/repos/acme/project/contents/src/new%20file.rs?ref={BASE}"),
                json!({"type":"file","size":8,"encoding":"base64","content":"b2xkIHRleHQ="}),
            ),
        ];
        let worker = std::thread::spawn(move || {
            for (path, response) in expected {
                let request = fixture
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("GitHub request");
                assert_eq!(request.method(), &Method::Get);
                assert_eq!(request.url(), path);
                assert_eq!(
                    header(&request, "Authorization").as_deref(),
                    Some("Bearer test-token")
                );
                let mut response = Response::from_string(response.to_string());
                if path.contains("/files?") {
                    response.add_header(tiny_http::Header::from_bytes(
                        "Link",
                        "<https://api.github.com/repos/acme/project/pulls/7/files?page=2&per_page=10>; rel=\"next\"",
                    ).unwrap());
                }
                request.respond(response).unwrap();
            }
        });
        let runtime = Runtime::new()?;
        let client = {
            let _runtime = runtime.enter();
            GitHub::builder()
                .base_uri(endpoint)?
                .personal_token("test-token".to_owned())
                .build()?
        };
        let target = Target {
            repository: "acme/project".into(),
            number: 7,
            head: HEAD.into(),
        };
        let pr = serde_json::from_value(json!({
            "title":"Review", "body":"", "state":"open", "labels":[{"name":"wrev"}],
            "base":{"sha":BASE},
            "head":{"sha":HEAD,"repo":{"full_name":"contributor/fork"}}
        }))?;
        let settings = ReviewConfig {
            model: "fixture".into(),
            github_timeout: Duration::from_secs(5),
            script: Default::default(),
        };
        let tools = register_tools(&client, &target, &pr, &settings)?;
        let script = r#"
            const tools = mcp.listTools("rust");
            if (tools.length !== 4) throw new Error("Unexpected capabilities");
            const pr = (await mcp.callTool("rust", "pulls_get", {})).structuredContent;
            if (pr.custom !== "kept") throw new Error("PR response was projected");
            const page = (await mcp.callTool("rust", "pulls_list_files", {page:1, per_page:10})).structuredContent;
            if (!page.next.includes("page=2")) throw new Error("Missing GitHub pagination");
            const file = await mcp.callTool("rust", "repos_get_content", {
                repository:"contributor/fork", ref:"1111111111111111111111111111111111111111",
                path:page.items[0].filename
            });
            console.log(file.structuredContent.content);
            for (const [side, expected] of [["head", "new text"], ["base", "old text"]]) {
                const rg = (await mcp.callTool("rust", "rg", {
                    side, pattern:"TEXT", paths:[page.items[0].filename], fixed_strings:true, ignore_case:true
                })).structuredContent;
                const found = rg.files[0].matches[0];
                if (found.text !== expected || found.line !== 1) throw new Error(JSON.stringify(rg));
            }
        "#;
        let runtime = Runtime::new()?;
        let result =
            runtime.block_on(tools.call("run_js", &json!({"code":script}).to_string(), false));
        worker.join().expect("GitHub fixture");
        assert_eq!(result?, json!("bmV3IHRleHQ=\n"));
        let outside_scope = json!({"code": format!(
            "await mcp.callTool('rust', 'repos_get_content', {{repository:'unrelated/repo', ref:'{HEAD}', path:'secret'}})"
        )});
        assert!(
            runtime
                .block_on(tools.call("run_js", &outside_scope.to_string(), false))
                .is_err()
        );
        assert!(file_route("acme/project", "../escape").is_err());
        Ok(())
    }
}
