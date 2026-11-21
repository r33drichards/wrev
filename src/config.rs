use anyhow::{Result, bail, ensure};
use minimal_mcp_agent::quickjs_tools::ScriptLimits;
use std::{
    env,
    net::IpAddr,
    num::{NonZeroU32, NonZeroUsize},
    str::FromStr,
    time::Duration,
};

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

pub fn required(key: &str) -> Result<String> {
    let value = env::var(key).map_err(|error| anyhow::anyhow!("{key}: {error}"))?;
    ensure!(!value.trim().is_empty(), "{key} is empty");
    Ok(value)
}

fn setting<T: FromStr>(key: &str, default: T) -> Result<T> {
    parse_setting(key, env::var(key), default)
}

fn parse_setting<T: FromStr>(
    key: &str,
    value: std::result::Result<String, env::VarError>,
    default: T,
) -> Result<T> {
    match value {
        Ok(value) => value.parse().map_err(|_| anyhow::anyhow!("Invalid {key}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(env::VarError::NotUnicode(_)) => bail!("{key} is not valid Unicode"),
    }
}

fn seconds(key: &str, default: u32) -> Result<Duration> {
    let seconds = setting(
        key,
        NonZeroU32::new(default).expect("positive timeout default"),
    )?;
    Ok(Duration::from_secs(u64::from(seconds.get())))
}

pub fn model_timeout() -> Result<Duration> {
    seconds("WREV_MODEL_TIMEOUT_SECS", 180)
}

#[cfg(unix)]
pub fn auth_timeout() -> Result<Duration> {
    seconds("WREV_AUTH_TIMEOUT_SECS", 30)
}

#[cfg(unix)]
pub struct OAuthCallbackConfig {
    pub timeout: Duration,
    pub read_timeout: Duration,
}

#[cfg(unix)]
impl OAuthCallbackConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            timeout: seconds("WREV_OAUTH_CALLBACK_TIMEOUT_SECS", 300)?,
            read_timeout: seconds("WREV_OAUTH_READ_TIMEOUT_SECS", 2)?,
        })
    }
}

fn positive_size(key: &str, default: usize) -> Result<usize> {
    Ok(setting(
        key,
        NonZeroUsize::new(default).expect("positive size default"),
    )?
    .get())
}

pub struct ReviewConfig {
    pub model: String,
    pub github_timeout: Duration,
    pub script: ScriptLimits,
}

impl ReviewConfig {
    pub fn from_env() -> Result<Self> {
        let defaults = ScriptLimits::default();
        Ok(Self {
            model: required("OPENAI_MODEL")?,
            github_timeout: seconds("WREV_GITHUB_TIMEOUT_SECS", 20)?,
            script: ScriptLimits {
                timeout: seconds(
                    "WREV_SCRIPT_TIMEOUT_SECS",
                    defaults.timeout.as_secs() as u32,
                )?,
                code_bytes: positive_size("WREV_SCRIPT_CODE_BYTES", defaults.code_bytes)?,
                output_bytes: positive_size("WREV_SCRIPT_OUTPUT_BYTES", defaults.output_bytes)?,
                memory_bytes: positive_size("WREV_SCRIPT_MEMORY_BYTES", defaults.memory_bytes)?,
                stack_bytes: positive_size("WREV_SCRIPT_STACK_BYTES", defaults.stack_bytes)?,
            },
        })
    }
}

pub struct ListenerConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub label: String,
    pub queue_capacity: usize,
    pub dedup_capacity: usize,
    pub body_bytes: usize,
}

impl ListenerConfig {
    pub fn from_env() -> Result<Self> {
        let label = setting("WREV_LABEL", String::from("wrev"))?;
        ensure!(!label.trim().is_empty(), "WREV_LABEL is empty");
        let port = setting("PORT", 8080u16)?;
        ensure!(port > 0, "PORT must be positive");
        let queue_capacity = positive_size("WREV_QUEUE_CAPACITY", 16)?;
        ensure!(
            queue_capacity <= isize::MAX as usize,
            "WREV_QUEUE_CAPACITY is too large"
        );
        Ok(Self {
            bind: setting("WREV_BIND_ADDR", IpAddr::from([0, 0, 0, 0]))?,
            port,
            label,
            queue_capacity,
            dedup_capacity: positive_size("WREV_DEDUP_CAPACITY", 4096)?,
            body_bytes: positive_size("WREV_WEBHOOK_BODY_BYTES", 1024 * 1024)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_independent_overrides_and_rejects_invalid_values() {
        let default = NonZeroU32::new(20).unwrap();
        assert_eq!(
            parse_setting("TEST", Err(env::VarError::NotPresent), default).unwrap(),
            default
        );
        assert_eq!(
            parse_setting("TEST", Ok("45".into()), default)
                .unwrap()
                .get(),
            45
        );
        assert!(parse_setting("TEST", Ok("0".into()), default).is_err());
        assert!(parse_setting("TEST", Ok("invalid".into()), default).is_err());
    }
}
