#[cfg(unix)]
mod auth;
#[cfg(not(unix))]
#[path = "auth_headless.rs"]
mod auth;
mod config;
#[cfg(feature = "mysql-backend")]
mod hosted_auth;
mod model;
mod review;
#[cfg(all(target_os = "wasi", target_env = "p2"))]
mod wasi_github;
#[cfg(all(target_os = "wasi", target_env = "p2"))]
mod wasi_http;

use anyhow::{Result, bail};
use std::env;

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
        Some("login") => auth::login(),
        #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
        Some("logout") => auth::logout(),
        Some("models") => {
            for model in model::models(&config::chatgpt_token()?)? {
                println!("{}", model["slug"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
        Some("wcgi") => review::wcgi(),
        Some("review") => review::manual(),
        None | Some("-h" | "--help") => {
            #[cfg(not(all(target_os = "wasi", target_env = "p2")))]
            println!("wrev [login | logout | models | wcgi | review OWNER/REPO PR_NUMBER]");
            #[cfg(all(target_os = "wasi", target_env = "p2"))]
            println!("wrev [models | review OWNER/REPO PR_NUMBER]");
            Ok(())
        }
        Some(other) => bail!("Unknown command: {other}"),
    }
}
