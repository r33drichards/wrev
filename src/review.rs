use crate::config::{self, ListenerConfig, ReviewConfig};
use anyhow::{Context, Result, anyhow, ensure};
use hmac::{Hmac, Mac};
use minimal_mcp_agent::{agent, quickjs_tools::QuickJsTools};
use octocrab::{Octocrab, Page};
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
    let token = config::required("WREV_GITHUB_TOKEN")?;
    Ok(Octocrab::builder().personal_token(token).build()?)
}

async fn pull(client: &Octocrab, repository: &str, number: u64, timeout: Duration) -> Result<Pull> {
    ensure!(
        allowed_repo(repository) && number > 0,
        "Invalid pull request identity"
    );
    let route = format!("/repos/{repository}/pulls/{number}");
    tokio::time::timeout(timeout, client.get(route, None::<&()>))
        .await?
        .context("Read pull request")
}

fn register_tools(
    client: &Octocrab,
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
            async move { Ok(tokio::time::timeout(timeout, client.get(route, None::<&()>)).await??) }
        },
    )?;

    let files_client = client.clone();
    tools.register_async_rust(
        "pulls_list_files",
        "List files for the authorized pull request. Returns an Octocrab page with items and pagination links.",
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
                let page: Page<Value> = tokio::time::timeout(
                    timeout, client.get(route, Some(&query)),
                ).await??;
                Ok(serde_json::to_value(page)?)
            }
        },
    )?;

    let file_client = client.clone();
    let base = (
        target.repository.to_ascii_lowercase(),
        pull.base.sha.clone(),
    );
    let head = head_repo.map(|repo| (repo.to_ascii_lowercase(), pull.head.sha.clone()));
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
                Ok(tokio::time::timeout(
                    timeout, client.get(route, Some(&[("ref", reference)])),
                ).await??)
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
    settings: &ReviewConfig,
    model_client: &reqwest::blocking::Client,
    target: &Target,
    required_label: Option<&str>,
    access_token: impl Fn() -> Result<String>,
) -> Result<()> {
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
    let definitions = QuickJsTools::definitions();
    let mut messages = Vec::new();
    agent::run_turn(
        &mut messages,
        include_str!("review_prompt.md"),
        None,
        |messages| {
            crate::model::prompt(
                model_client,
                &access_token()?,
                &settings.model,
                messages,
                &definitions,
            )
        },
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
    let settings = ReviewConfig::from_env()?;
    let current = runtime.block_on(pull(&client, &repository, number, settings.github_timeout))?;
    run_review(
        &runtime,
        &client,
        &settings,
        &crate::model::client()?,
        &Target {
            repository,
            number,
            head: current.head.sha,
        },
        None,
        config::chatgpt_token,
    )
}

pub(crate) fn header(request: &Request, name: &'static str) -> Option<String> {
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

pub fn listen() -> Result<()> {
    let secret = config::required("WREV_WEBHOOK_SECRET")?;
    let repositories: HashSet<_> = config::required("WREV_REPOSITORIES")?
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .collect();
    ensure!(
        !repositories.is_empty() && repositories.iter().all(|name| allowed_repo(name)),
        "Expected a comma-separated repository allowlist"
    );
    let runtime = Runtime::new()?;
    let client = authenticate(&runtime)?;
    let settings = ReviewConfig::from_env()?;
    let model_client = crate::model::client()?;
    let listener = ListenerConfig::from_env()?;
    let server =
        Server::http((listener.bind, listener.port)).map_err(|error| anyhow!("{error}"))?;
    let (queue, work) = mpsc::sync_channel::<Target>(listener.queue_capacity);
    let worker_label = listener.label.clone();
    let dedup_capacity = listener.dedup_capacity;
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let worker_seen = Arc::clone(&seen);
    #[cfg(feature = "mysql-backend")]
    let credentials = crate::hosted_auth::HostedAuth::from_env()?.map(Arc::new);
    #[cfg(feature = "mysql-backend")]
    let worker_credentials = credentials.clone();
    let access_token = move || {
        #[cfg(feature = "mysql-backend")]
        if let Some(store) = &worker_credentials {
            return store.access_token();
        }
        config::chatgpt_token()
    };
    std::thread::spawn(move || {
        let mut completed = VecDeque::new();
        for target in work {
            let result = run_review(
                &runtime,
                &client,
                &settings,
                &model_client,
                &target,
                Some(&worker_label),
                &access_token,
            );
            let mut seen = worker_seen.lock().expect("review state");
            if result.is_ok() {
                completed.push_back(target);
                if completed.len() > dedup_capacity
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
            #[cfg(feature = "mysql-backend")]
            (&Method::Put, "/admin/chatgpt-credentials") => credentials
                .as_ref()
                .map_or(404, |store| store.put(&mut request)),
            (&Method::Post, "/github/webhook") => webhook_status(
                &mut request,
                secret.as_bytes(),
                &repositories,
                &listener,
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
    listener: &ListenerConfig,
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
        .take((listener.body_bytes as u64).saturating_add(1))
        .read_to_end(&mut body)
        .is_err()
        || body.len() > listener.body_bytes
    {
        return 413;
    }
    if !signed(secret, &signature, &body) {
        return 401;
    }
    if event.as_deref() != Some("pull_request") {
        return 204;
    }
    let target = match labeled_target(&body, repositories, &listener.label) {
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
        let settings = ReviewConfig {
            model: "fixture".into(),
            github_timeout: Duration::from_secs(5),
            script: Default::default(),
        };
        let tools = register_tools(&client, &target, &pr, &settings)?;
        let script = r#"
            const tools = mcp.listTools("rust");
            if (tools.length !== 3) throw new Error("Unexpected capabilities");
            const pr = (await mcp.callTool("rust", "pulls_get", {})).structuredContent;
            if (pr.custom !== "kept") throw new Error("PR response was projected");
            const page = (await mcp.callTool("rust", "pulls_list_files", {page:1, per_page:10})).structuredContent;
            if (!page.next.includes("page=2")) throw new Error("Missing GitHub pagination");
            const file = await mcp.callTool("rust", "repos_get_content", {
                repository:"contributor/fork", ref:"1111111111111111111111111111111111111111",
                path:page.items[0].filename
            });
            console.log(file.structuredContent.content);
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
