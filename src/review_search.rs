use anyhow::{Result, ensure};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, sinks::UTF8};
use minimal_mcp_agent::quickjs_tools::{QuickJsTools, tool_timeout};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_PATH_BYTES: usize = 1024;
const MAX_PATTERN_BYTES: usize = 512;
pub(crate) const MAX_GITHUB_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 48 * 1024;
const MAX_SKIP_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Side {
    Base,
    #[default]
    Head,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    pattern: String,
    paths: Option<Vec<String>>,
    #[serde(default)]
    side: Side,
    #[serde(default)]
    fixed_strings: bool,
    #[serde(default)]
    ignore_case: bool,
    max_matches: Option<usize>,
}

pub(crate) struct Inventory {
    pub paths: Vec<String>,
    pub truncated: bool,
}

pub(crate) enum File {
    Bytes(Vec<u8>),
    Skipped(&'static str),
}

pub(crate) fn register<L, List, F, Read>(
    tools: &mut QuickJsTools,
    inventory: L,
    read: F,
) -> Result<()>
where
    L: Fn(Side, Duration) -> List + 'static,
    List: Future<Output = Result<Inventory>> + 'static,
    F: Fn(Side, String, usize, Duration) -> Read + 'static,
    Read: Future<Output = File> + 'static,
{
    let inventory = Rc::new(inventory);
    let read = Rc::new(read);
    tools.register_async_rust(
        "rg",
        "Search tracked text at the pinned PR head (default) or base with ripgrep. Omit paths for a repository-wide search; supply paths to search only specific files. Returns matching lines, skipped-file diagnostics, and a truncation flag.",
        json!({"type":"object","properties":{
            "pattern":{"type":"string","minLength":1,"maxLength":MAX_PATTERN_BYTES},
            "paths":{"type":"array","items":{"type":"string","minLength":1,"maxLength":MAX_PATH_BYTES},"minItems":1},
            "side":{"type":"string","enum":["head","base"],"default":"head"},
            "fixed_strings":{"type":"boolean","default":false},
            "ignore_case":{"type":"boolean","default":false},
            "max_matches":{"type":"integer","minimum":1}
        },"required":["pattern"],"additionalProperties":false}),
        move |args| {
            let inventory = inventory.clone();
            let read = read.clone();
            async move {
                let request = serde_json::from_value(Value::Object(args))?;
                let budget = tool_timeout(Duration::from_secs(120)).saturating_sub(Duration::from_millis(250));
                search(request, budget, move |side, budget| inventory(side, budget),
                    move |side, path, remaining, budget| read(side, path, remaining, budget)).await
            }
        },
    )
}

fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', '\0'])
        })
}

async fn search<L, List, F, Read>(
    request: Search,
    budget: Duration,
    inventory: L,
    read: F,
) -> Result<Value>
where
    L: Fn(Side, Duration) -> List,
    List: Future<Output = Result<Inventory>>,
    F: Fn(Side, String, usize, Duration) -> Read,
    Read: Future<Output = File>,
{
    ensure!(
        (1..=MAX_PATTERN_BYTES).contains(&request.pattern.len()),
        "Invalid rg pattern length"
    );
    ensure!(request.max_matches != Some(0), "Invalid rg match limit");
    if let Some(paths) = &request.paths {
        ensure!(
            !paths.is_empty() && paths.iter().all(|p| valid_path(p)),
            "Invalid repository file paths"
        );
    }

    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(request.fixed_strings)
        .case_insensitive(request.ignore_case)
        .line_terminator(Some(b'\n'))
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build(&request.pattern)?;
    let deadline = Instant::now() + budget;
    let mut stop_reason = None;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let inventory = match tokio::time::timeout(remaining, inventory(request.side, remaining)).await
    {
        Ok(Ok(inventory)) => inventory,
        Ok(Err(error)) if Instant::now() < deadline => return Err(error),
        _ => {
            stop_reason = Some("deadline");
            Inventory {
                paths: Vec::new(),
                truncated: true,
            }
        }
    };
    let inventory_incomplete = inventory.truncated;
    let (paths, allowed) = match request.paths {
        Some(paths) => (
            paths,
            Some(inventory.paths.into_iter().collect::<HashSet<_>>()),
        ),
        None => (inventory.paths, None),
    };
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut skipped_count = 0;
    let mut skip_bytes = 0;
    let mut count = 0;
    let mut searched_bytes = 0;
    let mut result_bytes = 0;

    for (index, path) in paths.into_iter().enumerate() {
        if stop_reason.is_some() {
            break;
        }
        if index % 32 == 0 {
            tokio::task::yield_now().await;
        }
        if !seen.insert(path.clone()) {
            continue;
        }
        if result_bytes >= MAX_RESULT_BYTES || count >= request.max_matches.unwrap_or(usize::MAX) {
            stop_reason = Some("output");
            break;
        }
        if searched_bytes >= MAX_SCAN_BYTES {
            stop_reason = Some("scan_bytes");
            break;
        }
        let remaining_time = deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            stop_reason = Some("deadline");
            break;
        }
        let input = if !valid_path(&path) {
            File::Skipped("invalid_path")
        } else if allowed
            .as_ref()
            .is_some_and(|entries| !entries.contains(&path))
        {
            File::Skipped(if inventory_incomplete {
                "not_indexed"
            } else {
                "not_a_file"
            })
        } else {
            match tokio::time::timeout(
                remaining_time,
                read(
                    request.side,
                    path.clone(),
                    MAX_SCAN_BYTES - searched_bytes,
                    remaining_time,
                ),
            )
            .await
            {
                Ok(input) if Instant::now() < deadline => input,
                _ => {
                    stop_reason = Some("deadline");
                    break;
                }
            }
        };
        let reason = match input {
            File::Bytes(bytes) if bytes.len() > MAX_SCAN_BYTES - searched_bytes => {
                Some("too_large")
            }
            File::Bytes(bytes) => {
                searched_bytes += bytes.len();
                if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
                    Some("not_utf8_text")
                } else {
                    let mut matches = Vec::new();
                    let file_overhead = serde_json::to_vec(&path)?.len() + 32;
                    searcher.search_slice(&matcher, &bytes, UTF8(|line_number, line| {
                        if count >= request.max_matches.unwrap_or(usize::MAX) {
                            stop_reason = Some("output");
                            return Ok(false);
                        }
                        if Instant::now() >= deadline {
                            stop_reason = Some("deadline");
                            return Ok(false);
                        }
                        let line = line.trim_end_matches(['\r', '\n']);
                        let mut end = line.len().min(MAX_RESULT_BYTES);
                        while !line.is_char_boundary(end) { end -= 1; }
                        let overhead = if matches.is_empty() { file_overhead } else { 0 };
                        loop {
                            let record = json!({"line":line_number,"text":&line[..end],"line_truncated":end < line.len()});
                            let cost = record.to_string().len() + overhead + 1;
                            if result_bytes + cost <= MAX_RESULT_BYTES {
                                matches.push(record);
                                result_bytes += cost;
                                count += 1;
                                if end < line.len() { stop_reason = Some("output"); }
                                return Ok(stop_reason.is_none());
                            }
                            if end == 0 || count > 0 {
                                stop_reason = Some("output");
                                return Ok(false);
                            }
                            end /= 2;
                            while !line.is_char_boundary(end) { end -= 1; }
                        }
                    }))?;
                    if !matches.is_empty() {
                        files.push(json!({"path":path,"matches":matches}));
                    }
                    None
                }
            }
            File::Skipped(reason) => Some(reason),
        };
        if let Some(reason) = reason {
            skipped_count += 1;
            let sample = json!({"path":path,"reason":reason});
            let cost = sample.to_string().len() + 1;
            if skip_bytes + cost <= MAX_SKIP_BYTES {
                skipped.push(sample);
                skip_bytes += cost;
            }
        }
        if stop_reason.is_some() {
            break;
        }
    }
    let stop_reason = stop_reason.or(inventory_incomplete.then_some("inventory"));
    Ok(json!({
        "files": files, "skipped": skipped, "skipped_count": skipped_count,
        "truncated": stop_reason.is_some(), "stop_reason": stop_reason,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::ready;

    #[tokio::test]
    async fn search_limits_matches_and_reports_skipped_inputs() -> Result<()> {
        let req = serde_json::from_value(json!({
            "pattern": "x.y",
            "paths": ["binary", "large", "text", "text", "more"],
            "fixed_strings": true,
            "ignore_case": true,
            "max_matches": 2,
        }))?;
        let reads = std::cell::Cell::new(0);
        let result = search(
            req,
            Duration::from_secs(1),
            |_, _| {
                ready(Ok(Inventory {
                    paths: ["binary", "large", "text", "more"]
                        .map(str::to_owned)
                        .to_vec(),
                    truncated: false,
                }))
            },
            |_, path, _, _| {
                reads.set(reads.get() + 1);
                if path == "large" {
                    return ready(File::Skipped("too_large"));
                }
                ready(File::Bytes(match path.as_str() {
                    "binary" => b"x.y\0".to_vec(),
                    "text" => format!("x-y\nX.Y {}\nx.y\n", "é".repeat(300)).into_bytes(),
                    "more" => b"x.y\n".to_vec(),
                    _ => unreachable!(),
                }))
            },
        )
        .await?;
        assert_eq!(reads.get(), 3);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["skipped"][0]["reason"], "not_utf8_text");
        assert_eq!(result["skipped"][1]["reason"], "too_large");
        let lines = result["files"][0]["matches"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["line"], 2);
        assert_eq!(lines[0]["line_truncated"], false);
        assert_eq!(
            lines[1],
            json!({"line":3,"text":"x.y","line_truncated":false})
        );
        Ok(())
    }

    #[tokio::test]
    async fn repository_search_does_not_cap_path_or_match_counts() -> Result<()> {
        let req = serde_json::from_value(json!({"pattern":"needle"}))?;
        let result = search(
            req,
            Duration::from_secs(1),
            |_, _| {
                ready(Ok(Inventory {
                    paths: (0..100).map(|i| format!("file-{i:03}")).collect(),
                    truncated: false,
                }))
            },
            |_, _, _, _| ready(File::Bytes(b"needle\n".to_vec())),
        )
        .await?;
        assert_eq!(result["files"].as_array().unwrap().len(), 100);
        assert_eq!(result["truncated"], false);
        Ok(())
    }

    #[tokio::test]
    async fn slow_read_keeps_earlier_matches() -> Result<()> {
        let req = serde_json::from_value(json!({"pattern":"needle"}))?;
        let result = search(
            req,
            Duration::from_millis(100),
            |_, _| {
                ready(Ok(Inventory {
                    paths: vec!["first".into(), "slow".into()],
                    truncated: false,
                }))
            },
            |_, path, _, budget| async move {
                if path == "slow" {
                    assert!(budget <= Duration::from_millis(100));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                File::Bytes(b"needle\n".to_vec())
            },
        )
        .await?;
        assert_eq!(result["files"][0]["path"], "first");
        assert_eq!(result["truncated"], true);
        assert_eq!(result["stop_reason"], "deadline");
        Ok(())
    }

    #[tokio::test]
    async fn invalid_search_is_rejected_before_any_file_reads() {
        for args in [
            json!({"pattern":"a","paths":["valid.rs","a/../../outside"]}),
            json!({"pattern":"[","paths":["valid.rs"]}),
            json!({"pattern":"a","paths":["valid.rs"],"max_matches":0}),
            json!({"pattern":"a".repeat(513),"paths":["valid.rs"]}),
        ] {
            let req = serde_json::from_value(args).unwrap();
            assert!(
                search(
                    req,
                    Duration::from_secs(1),
                    |_, _| -> std::future::Ready<Result<Inventory>> {
                        panic!("invalid requests must not list")
                    },
                    |_, _, _, _| -> std::future::Ready<File> {
                        panic!("invalid requests must not read")
                    }
                )
                .await
                .is_err()
            );
        }
    }
}
