use anyhow::{Result, ensure};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, sinks::UTF8};
use minimal_mcp_agent::quickjs_tools::QuickJsTools;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, future::Future, rc::Rc};

const MAX_PATHS: usize = 8;
const MAX_PATH_BYTES: usize = 1024;
const MAX_PATTERN_BYTES: usize = 512;
const MAX_MATCHES: usize = 50;
pub(crate) const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_LINE_BYTES: usize = 384;

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
    paths: Vec<String>,
    #[serde(default)]
    side: Side,
    #[serde(default)]
    fixed_strings: bool,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default = "default_limit")]
    max_matches: usize,
}

fn default_limit() -> usize {
    20
}

pub(crate) enum File {
    Bytes(Vec<u8>),
    Skipped(&'static str),
}

pub(crate) fn register<F, Fut>(tools: &mut QuickJsTools, read: F) -> Result<()>
where
    F: Fn(Side, String) -> Fut + 'static,
    Fut: Future<Output = File> + 'static,
{
    let read = Rc::new(read);
    tools.register_async_rust(
        "rg",
        "Search explicit repository-relative files at the PR's pinned head (default) or base. Uses ripgrep regexes or literal text; returns bounded matching lines with 1-based line numbers, skipped files, and a truncation flag. Does not walk directories or expand globs.",
        json!({"type":"object","properties":{
            "pattern":{"type":"string","minLength":1,"maxLength":MAX_PATTERN_BYTES},
            "paths":{"type":"array","items":{"type":"string","minLength":1,"maxLength":MAX_PATH_BYTES},"minItems":1,"maxItems":MAX_PATHS},
            "side":{"type":"string","enum":["head","base"],"default":"head"},
            "fixed_strings":{"type":"boolean","default":false},
            "ignore_case":{"type":"boolean","default":false},
            "max_matches":{"type":"integer","minimum":1,"maximum":MAX_MATCHES,"default":default_limit()}
        },"required":["pattern","paths"],"additionalProperties":false}),
        move |args| {
            let read = read.clone();
            async move {
                let request = serde_json::from_value(Value::Object(args))?;
                search(request, move |side, path| read(side, path)).await
            }
        },
    )
}

async fn search<F, Fut>(request: Search, read: F) -> Result<Value>
where
    F: Fn(Side, String) -> Fut,
    Fut: Future<Output = File>,
{
    ensure!(
        (1..=MAX_PATTERN_BYTES).contains(&request.pattern.len()),
        "Invalid rg pattern length"
    );
    ensure!(
        (1..=MAX_PATHS).contains(&request.paths.len()),
        "Invalid rg path count"
    );
    ensure!(
        (1..=MAX_MATCHES).contains(&request.max_matches),
        "Invalid rg match limit"
    );
    // Validate the entire request before touching either backend.
    for path in &request.paths {
        ensure!(
            path.len() <= MAX_PATH_BYTES
                && path.split('/').all(|part| {
                    !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', '\0'])
                }),
            "Invalid repository file path"
        );
    }

    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(request.fixed_strings)
        .case_insensitive(request.ignore_case)
        .line_terminator(Some(b'\n'))
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build(&request.pattern)?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut count = 0;
    let mut truncated = false;

    for path in request.paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        let bytes = match read(request.side, path.clone()).await {
            File::Bytes(bytes) if bytes.len() > MAX_FILE_BYTES => {
                skipped.push(json!({"path":path,"reason":"too_large"}));
                continue;
            }
            File::Bytes(bytes) if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() => {
                skipped.push(json!({"path":path,"reason":"not_utf8_text"}));
                continue;
            }
            File::Bytes(bytes) => bytes,
            File::Skipped(reason) => {
                skipped.push(json!({"path":path,"reason":reason}));
                continue;
            }
        };
        let mut matches = Vec::new();
        searcher.search_slice(
            &matcher,
            &bytes,
            UTF8(|line_number, line| {
                if count == request.max_matches {
                    truncated = true;
                    return Ok(false);
                }
                let line = line.trim_end_matches(['\r', '\n']);
                let mut end = line.len().min(MAX_LINE_BYTES);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                matches.push(json!({"line":line_number,"text":&line[..end],"line_truncated":end < line.len()}));
                count += 1;
                Ok(true)
            }),
        )?;
        if !matches.is_empty() {
            files.push(json!({"path":path,"matches":matches}));
        }
        if truncated {
            break;
        }
    }
    Ok(json!({
        "files":files,
        "skipped":skipped,
        "truncated":truncated,
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
        let result = search(req, |_, path| {
            reads.set(reads.get() + 1);
            ready(File::Bytes(match path.as_str() {
                "binary" => b"x.y\0".to_vec(),
                "large" => vec![b'x'; MAX_FILE_BYTES + 1],
                "text" => format!("x-y\nX.Y {}\nx.y\n", "é".repeat(300)).into_bytes(),
                "more" => b"x.y\n".to_vec(),
                _ => unreachable!(),
            }))
        })
        .await?;
        assert_eq!(reads.get(), 4);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["skipped"][0]["reason"], "not_utf8_text");
        assert_eq!(result["skipped"][1]["reason"], "too_large");
        let lines = result["files"][0]["matches"].as_array().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["line"], 2);
        assert_eq!(lines[0]["line_truncated"], true);
        assert_eq!(
            lines[1],
            json!({"line":3,"text":"x.y","line_truncated":false})
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_search_is_rejected_before_any_file_reads() {
        for args in [
            json!({"pattern":"a","paths":["valid.rs","a/../../outside"]}),
            json!({"pattern":"[","paths":["valid.rs"]}),
            json!({"pattern":"a","paths":["valid.rs"],"max_matches":51}),
            json!({"pattern":"a","paths":vec!["valid.rs"; 9]}),
            json!({"pattern":"a".repeat(513),"paths":["valid.rs"]}),
        ] {
            let req = serde_json::from_value(args).unwrap();
            assert!(
                search(req, |_, _| -> std::future::Ready<File> {
                    panic!("invalid requests must not read")
                })
                .await
                .is_err()
            );
        }
    }
}
