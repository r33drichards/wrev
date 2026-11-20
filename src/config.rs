use anyhow::{Result, bail, ensure};
use std::env;

pub fn chatgpt_token() -> Result<String> {
    match env::var("CHATGPT_ACCESS_TOKEN") {
        Ok(token) => {
            ensure!(!token.trim().is_empty(), "CHATGPT_ACCESS_TOKEN is empty");
            Ok(token)
        }
        Err(env::VarError::NotPresent) => crate::auth::access_token(),
        Err(_) => bail!("CHATGPT_ACCESS_TOKEN is not valid Unicode"),
    }
}
