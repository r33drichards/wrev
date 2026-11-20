use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use minimal_mcp_agent::{agent, quickjs_tools::QuickJsTools};
use octocrab::Octocrab;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{HashSet, VecDeque},
    env,
    io::Read,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use tiny_http::{Method, Request, Response, Server, StatusCode};
use tokio::runtime::Runtime;

const LABEL: &str = "wrev";
const FILES_PER_PAGE: usize = 10;
const MAX_PAGE: u64 = 300;
const MAX_FILE_BYTES: usize = 64 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

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
    title: String,
    body: Option<String>,
    state: String,
    base: Ref,
    head: Ref,
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct PullEvent {
    action: String,
    number: u64,
    label: Option<Label>,
    repository: Repository,
    pull_request: EventPull,
}

#[derive(Deserialize)]
struct EventPull {
    head: Ref,
}

#[derive(Deserialize)]
struct FileContent {
    #[serde(rename = "type")]
    kind: String,
    size: usize,
    encoding: Option<String>,
    content: Option<String>,
}

fn allowed_repo(name: &str) -> bool {
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

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn authenticate(runtime: &Runtime) -> Result<Octocrab> {
    let _runtime = runtime.enter();
    let token = env::var("WREV_GITHUB_TOKEN").context("Set WREV_GITHUB_TOKEN")?;
    ensure!(!token.trim().is_empty(), "WREV_GITHUB_TOKEN is empty");
    Ok(Octocrab::builder().personal_token(token).build()?)
}

async fn pull(client: &Octocrab, repository: &str, number: u64) -> Result<Pull> {
    ensure!(
        allowed_repo(repository) && number > 0,
        "Invalid pull request identity"
    );
    let route = format!("/repos/{repository}/pulls/{number}");
    tokio::time::timeout(HTTP_TIMEOUT, client.get(route, None::<&()>))
        .await?
        .context("Read pull request")
}

fn register_tools(client: &Octocrab, target: &Target, pull: &Pull) -> Result<QuickJsTools> {
    ensure!(
        valid_sha(&pull.base.sha) && valid_sha(&pull.head.sha),
        "Invalid PR commit SHA"
    );
    let head_repo = pull.head.repo.as_ref().map(|repo| repo.full_name.clone());
    if let Some(repo) = &head_repo {
        ensure!(allowed_repo(repo), "Invalid head repository");
    }

    let mut tools = QuickJsTools::default();
    let context = json!({
        "repository": target.repository, "number": target.number,
        "title": pull.title, "body": pull.body,
        "base": pull.base.sha, "head": pull.head.sha
    });
    tools.register_rust(
        "pr_context",
        "Read review identity and PR metadata",
        json!({"type":"object","additionalProperties":false}),
        move |_| Ok(context.clone()),
    )?;

    let files_route = format!("/repos/{}/pulls/{}/files", target.repository, target.number);
    let files_client = client.clone();
    tools.register_async_rust(
        "list_changed_files",
        "Read one page of changed files and diff excerpts. Keep requesting pages until nextPage is null.",
        json!({"type":"object","properties":{"page":{"type":"integer","minimum":1,"maximum":MAX_PAGE}},"required":["page"],"additionalProperties":false}),
        move |args| {
            let client = files_client.clone();
            let route = files_route.clone();
            async move {
                let page = args.get("page").and_then(Value::as_u64).filter(|page| (1..=MAX_PAGE).contains(page)).context("Invalid page")?;
                let files: Vec<Value> = tokio::time::timeout(
                    HTTP_TIMEOUT,
                    client.get(route, Some(&[("page", page), ("per_page", FILES_PER_PAGE as u64)])),
                ).await??;
                let next = (files.len() == FILES_PER_PAGE && page < MAX_PAGE).then_some(page + 1);
                let files: Vec<_> = files.into_iter().map(|file| {
                    let patch = file["patch"].as_str();
                    json!({
                        "filename": file["filename"], "previousFilename": file["previous_filename"],
                        "status": file["status"], "additions": file["additions"], "deletions": file["deletions"],
                        "patch": patch.map(|text| text.chars().take(4000).collect::<String>()),
                        "patchTruncated": patch.is_some_and(|text| text.chars().nth(4000).is_some()),
                    })
                }).collect();
                Ok(json!({"files": files, "nextPage": next, "githubFileLimitReached": page == MAX_PAGE && files.len() == FILES_PER_PAGE}))
            }
        },
    )?;

    let file_client = client.clone();
    let base = (target.repository.clone(), pull.base.sha.clone());
    let head = head_repo.map(|repo| (repo, pull.head.sha.clone()));
    tools.register_async_rust(
        "read_file",
        "Read a UTF-8 repository file at the verified base or head commit, up to 64 KiB",
        json!({"type":"object","properties":{"side":{"enum":["base","head"]},"path":{"type":"string"}},"required":["side","path"],"additionalProperties":false}),
        move |args| {
            let client = file_client.clone();
            let base = base.clone();
            let head = head.clone();
            async move {
                let (repository, sha) = match args.get("side").and_then(Value::as_str) {
                    Some("base") => base,
                    Some("head") => head.context("Head repository is unavailable")?,
                    _ => anyhow::bail!("Invalid side"),
                };
                let path = args.get("path").and_then(Value::as_str).context("Missing path")?;
                let route = file_route(&repository, path)?;
                let file: FileContent = tokio::time::timeout(
                    HTTP_TIMEOUT, client.get(route, Some(&[("ref", sha)])),
                ).await??;
                ensure!(file.kind == "file" && file.size <= MAX_FILE_BYTES && file.encoding.as_deref() == Some("base64"), "File is unavailable or too large");
                let encoded = file.content.context("Missing file contents")?;
                let bytes = STANDARD.decode(encoded.lines().collect::<String>())?;
                ensure!(bytes.len() <= MAX_FILE_BYTES, "File is too large");
                let content = String::from_utf8(bytes)?;
                Ok(json!({"path": path, "content": content}))
            }
        },
    )?;
    Ok(tools)
}

fn file_route(repository: &str, path: &str) -> Result<String> {
    ensure!(
        path.len() <= 1024
            && path.split('/').all(|part| {
                !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', '\0'])
            }),
        "Invalid repository file path"
    );
    let mut url = reqwest::Url::parse(&format!(
        "https://api.github.com/repos/{repository}/contents"
    ))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("Invalid file URL"))?
        .extend(path.split('/'));
    Ok(url.path().to_owned())
}

fn run_review(
    runtime: &Runtime,
    client: &Octocrab,
    model_name: &str,
    target: &Target,
    require_label: bool,
) -> Result<()> {
    let pr = runtime.block_on(pull(client, &target.repository, target.number))?;
    let eligible = |pr: &Pull| {
        pr.state == "open"
            && pr.head.sha == target.head
            && (!require_label || pr.labels.iter().any(|label| label.name == LABEL))
    };
    ensure!(
        eligible(&pr),
        "Pull request is closed, changed, or no longer labeled"
    );
    let tools = register_tools(client, target, &pr)?;
    let definitions = QuickJsTools::definitions();
    let token = crate::config::chatgpt_token()?;
    let mut messages = Vec::new();
    agent::run_turn(
        &mut messages,
        include_str!("review_prompt.md"),
        20,
        |messages| crate::model::prompt(&token, model_name, messages, &definitions),
        |name, args| runtime.block_on(tools.call(name, args, false)),
        |_| {},
    )?;
    let review = messages
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
        .context("Model returned no final review")?;
    let final_pr = runtime.block_on(pull(client, &target.repository, target.number))?;
    ensure!(
        eligible(&final_pr) && final_pr.base.sha == pr.base.sha,
        "Pull request changed during review"
    );
    println!(
        "{}",
        json!({"repository": target.repository, "number": target.number,
        "head": target.head, "review": review})
    );
    Ok(())
}

pub fn manual() -> Result<()> {
    let mut args = env::args().skip(2);
    let repository = args
        .next()
        .context("Usage: wrev review OWNER/REPO PR_NUMBER")?;
    let number = args.next().context("Missing PR number")?.parse()?;
    ensure!(args.next().is_none(), "Unexpected arguments");
    let runtime = Runtime::new()?;
    let client = authenticate(&runtime)?;
    let current = runtime.block_on(pull(&client, &repository, number))?;
    let model = env::var("OPENAI_MODEL").context("Set OPENAI_MODEL")?;
    run_review(
        &runtime,
        &client,
        &model,
        &Target {
            repository,
            number,
            head: current.head.sha,
        },
        false,
    )
}

fn header(request: &Request, name: &'static str) -> Option<String> {
    let mut matches = request
        .headers()
        .iter()
        .filter(|field| field.field.equiv(name));
    let first = matches.next()?.value.as_str().to_owned();
    matches.next().is_none().then_some(first)
}

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

fn labeled_target(body: &[u8], repositories: &HashSet<String>) -> Result<Option<Target>> {
    let event: PullEvent = serde_json::from_slice(body)?;
    if event.action != "labeled"
        || !event.label.is_some_and(|label| label.name == LABEL)
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

pub fn listen() -> Result<()> {
    let secret = env::var("WREV_WEBHOOK_SECRET").context("Set WREV_WEBHOOK_SECRET")?;
    ensure!(!secret.trim().is_empty(), "WREV_WEBHOOK_SECRET is empty");
    let repositories: HashSet<_> = env::var("WREV_REPOSITORIES")
        .context("Set WREV_REPOSITORIES")?
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    ensure!(
        !repositories.is_empty() && repositories.iter().all(|name| allowed_repo(name)),
        "Expected a comma-separated repository allowlist"
    );
    let runtime = Runtime::new()?;
    let client = authenticate(&runtime)?;
    let model = env::var("OPENAI_MODEL").context("Set OPENAI_MODEL")?;
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_owned())
        .parse()?;
    let server = Server::http(("0.0.0.0", port)).map_err(|error| anyhow!("{error}"))?;
    let (queue, work) = mpsc::sync_channel::<Target>(16);
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let worker_seen = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut completed = VecDeque::new();
        for target in work {
            let result = run_review(&runtime, &client, &model, &target, true);
            let mut seen = worker_seen.lock().expect("review state");
            if result.is_ok() {
                completed.push_back(target);
                if completed.len() > 4096
                    && let Some(old) = completed.pop_front()
                {
                    seen.remove(&old);
                }
            } else {
                eprintln!("Review failed for {}#{}", target.repository, target.number);
                seen.remove(&target);
            }
        }
    });

    for mut request in server.incoming_requests() {
        let status = match (request.method(), request.url()) {
            (&Method::Get, "/health") => 200,
            (&Method::Post, "/github/webhook") => webhook_status(
                &mut request,
                secret.as_bytes(),
                &repositories,
                &queue,
                &seen,
            ),
            _ => 404,
        };
        let _ = request.respond(Response::empty(StatusCode(status)));
    }
    Ok(())
}

fn webhook_status(
    request: &mut Request,
    secret: &[u8],
    repositories: &HashSet<String>,
    queue: &mpsc::SyncSender<Target>,
    seen: &Mutex<HashSet<Target>>,
) -> u16 {
    let Some(signature) = header(request, "X-Hub-Signature-256") else {
        return 401;
    };
    let event = header(request, "X-GitHub-Event");
    let mut body = Vec::new();
    if request
        .as_reader()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut body)
        .is_err()
        || body.len() > 1024 * 1024
    {
        return 413;
    }
    if !signed(secret, &signature, &body) {
        return 401;
    }
    if event.as_deref() != Some("pull_request") {
        return 204;
    }
    let target = match labeled_target(&body, repositories) {
        Ok(Some(target)) => target,
        Ok(None) => return 204,
        Err(_) => return 400,
    };
    let mut seen = seen.lock().expect("review state");
    if seen.contains(&target) {
        return 202;
    }
    match queue.try_send(target.clone()) {
        Ok(()) => {
            seen.insert(target);
            202
        }
        Err(_) => 503,
    }
}

#[cfg(test)]
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
        assert_eq!(labeled_target(&body, &repo)?.unwrap().head, HEAD);
        assert!(labeled_target(&body, &HashSet::new())?.is_none());
        event["label"]["name"] = json!("wreview");
        assert!(labeled_target(&serde_json::to_vec(&event)?, &repo)?.is_none());
        Ok(())
    }

    #[test]
    fn quickjs_reads_scoped_github_data_using_only_authenticated_gets() -> Result<()> {
        let fixture = Server::http("127.0.0.1:0").map_err(|error| anyhow!("{error}"))?;
        let endpoint = format!("http://{}", fixture.server_addr());
        let expected = [
            (
                "/repos/acme/project/pulls/7/files?page=1&per_page=10".to_owned(),
                json!([
                    {"filename":"src/new file.rs","status":"modified","patch":"+new text"}
                ]),
            ),
            (
                format!("/repos/contributor/fork/contents/src/new%20file.rs?ref={HEAD}"),
                json!({"type":"file","size":8,"encoding":"base64","content":STANDARD.encode("new text")}),
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
                request
                    .respond(Response::from_string(response.to_string()))
                    .unwrap();
            }
        });
        let runtime = Runtime::new()?;
        let client = {
            let _runtime = runtime.enter();
            Octocrab::builder()
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
        let tools = register_tools(&client, &target, &pr)?;
        let script = r#"
            const tools = mcp.listTools("rust");
            if (tools.length !== 3) throw new Error("Unexpected capabilities");
            const page = (await mcp.callTool("rust", "list_changed_files", {page:1})).structuredContent;
            const file = await mcp.callTool("rust", "read_file", {side:"head", path:page.files[0].filename});
            console.log(file.structuredContent.content);
        "#;
        let runtime = Runtime::new()?;
        let result =
            runtime.block_on(tools.call("run_js", &json!({"code":script}).to_string(), false));
        worker.join().expect("GitHub fixture");
        assert_eq!(result?, json!("new text\n"));
        assert!(file_route("acme/project", "../escape").is_err());
        Ok(())
    }
}
