use crate::wasi_http::{self, Response};
use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{io::BufReader, time::Duration};
use wasi::http::types::Method;

#[derive(Clone)]
pub struct GitHub(String);

impl GitHub {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    fn request(
        &self,
        mut route: String,
        query: Option<&(impl Serialize + ?Sized)>,
        timeout: Duration,
    ) -> Result<Response> {
        if let Some(query) = query {
            let query = serde_urlencoded::to_string(query)?;
            if !query.is_empty() {
                route.push('?');
                route.push_str(&query);
            }
        }
        wasi_http::request(
            Method::Get,
            "api.github.com",
            &route,
            &self.0,
            None,
            timeout,
        )
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        route: String,
        query: Option<&impl Serialize>,
        timeout: Duration,
    ) -> Result<T> {
        Ok(serde_json::from_reader(BufReader::new(
            self.request(route, query, timeout)?,
        ))?)
    }

    pub fn files(&self, route: String, query: &[(&str, u64)], timeout: Duration) -> Result<Value> {
        let response = self.request(route, Some(query), timeout)?;
        let mut page = json!({"next":null,"prev":null,"first":null,"last":null,
                              "incomplete_results":null,"total_count":null});
        for field in response.links.iter().flat_map(|header| header.split(',')) {
            if let Some((uri, relation)) = field.trim().split_once(">; rel=\"")
                && let Some(uri) = uri.strip_prefix('<')
                && let Some(relation) = relation.strip_suffix('"')
                && let Some(value) = page.get_mut(relation)
                && matches!(relation, "next" | "prev" | "first" | "last")
            {
                *value = json!(uri);
            }
        }
        page["items"] = json!(serde_json::from_reader::<_, Vec<Value>>(BufReader::new(
            response
        ))?);
        Ok(page)
    }
}
