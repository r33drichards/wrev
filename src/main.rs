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

use anyhow::{Result, bail};
use std::env;

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("login") => auth::login(),
        Some("logout") => auth::logout(),
        Some("models") => {
            for model in model::models(&config::chatgpt_token()?)? {
                println!("{}", model["slug"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Some("listen") => review::listen(),
        Some("review") => review::manual(),
        None | Some("-h" | "--help") => {
            println!("wrev [login | logout | models | listen | review OWNER/REPO PR_NUMBER]");
            Ok(())
        }
        Some(other) => bail!("Unknown command: {other}"),
    }
}
