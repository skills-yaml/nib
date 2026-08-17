use super::{LiveMode, NETWORK_PROVIDERS};
use std::env;
use std::path::PathBuf;
use std::time::Duration;

const MAX_RESULTS_PATH_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub(super) struct LiveLimits {
    pub max_logical_requests: usize,
    pub max_output_tokens_per_request: u32,
    pub max_scenario_duration: Duration,
    pub max_provider_duration: Duration,
    pub allow_unpriced: bool,
}

#[derive(Clone)]
pub(super) struct LiveSettings {
    pub mode: LiveMode,
    pub providers: Vec<String>,
    pub results_dir: PathBuf,
    pub limits: LiveLimits,
    pub meta_base_url: Option<String>,
    pub(super) sensitive_values: Vec<String>,
}

impl std::fmt::Debug for LiveSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveSettings")
            .field("mode", &self.mode)
            .field("providers", &self.providers)
            .field("results_dir", &self.results_dir)
            .field("limits", &self.limits)
            .field(
                "meta_base_url",
                &self.meta_base_url.as_ref().map(|_| "<redacted>"),
            )
            .field("sensitive_value_count", &self.sensitive_values.len())
            .finish()
    }
}

impl LiveSettings {
    pub fn from_environment() -> Result<Self, String> {
        require_flag("NIB_LIVE_TESTS")?;
        let mode = LiveMode::parse(&required("NIB_LIVE_MODE")?)?;
        if mode.makes_generation_requests() {
            require_flag("NIB_LIVE_ACK_COSTS")?;
        }
        let providers = parse_providers(&required("NIB_LIVE_PROVIDER")?)?;
        let results_dir = env::var_os("NIB_LIVE_RESULTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/llm-live"));
        let results_path = results_dir.to_string_lossy();
        if results_path.is_empty()
            || results_path.len() > MAX_RESULTS_PATH_BYTES
            || results_path.contains('\0')
        {
            return Err("NIB_LIVE_RESULTS_DIR is invalid".to_string());
        }

        let max_logical_requests = parse_usize("NIB_LIVE_MAX_REQUESTS", 10_000, 1, 100_000)?;
        let max_output_tokens_per_request = parse_u32("NIB_LIVE_MAX_OUTPUT_TOKENS", 64, 1, 512)?;
        let max_scenario_duration =
            Duration::from_secs(parse_u64("NIB_LIVE_SCENARIO_TIMEOUT_SECS", 120, 5, 600)?);
        let max_provider_duration = Duration::from_secs(parse_u64(
            "NIB_LIVE_PROVIDER_TIMEOUT_SECS",
            3_600,
            60,
            14_400,
        )?);
        let allow_unpriced = optional_flag("NIB_LIVE_ALLOW_UNPRICED")?;
        let meta_base_url = env::var("NIB_LIVE_META_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty());

        let mut sensitive_values = NETWORK_PROVIDERS
            .iter()
            .filter_map(|provider| credential_env(provider))
            .filter_map(|name| env::var(name).ok())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if let Some(value) = &meta_base_url {
            sensitive_values.push(value.clone());
        }
        sensitive_values.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
        sensitive_values.dedup();

        for provider in &providers {
            let credential_name = credential_env(provider)
                .ok_or_else(|| format!("provider '{provider}' has no live credential mapping"))?;
            if env::var(credential_name)
                .ok()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(format!(
                    "provider '{provider}' is blocked_auth because {credential_name} is missing"
                ));
            }
            if provider == "meta" && meta_base_url.is_none() {
                return Err(
                    "provider 'meta' is blocked_configuration because NIB_LIVE_META_BASE_URL is missing"
                        .to_string(),
                );
            }
        }

        Ok(Self {
            mode,
            providers,
            results_dir,
            limits: LiveLimits {
                max_logical_requests,
                max_output_tokens_per_request,
                max_scenario_duration,
                max_provider_duration,
                allow_unpriced,
            },
            meta_base_url,
            sensitive_values,
        })
    }

    pub fn credential(&self, provider: &str) -> Result<String, String> {
        let name = credential_env(provider)
            .ok_or_else(|| format!("provider '{provider}' has no credential mapping"))?;
        env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("provider '{provider}' is blocked_auth"))
    }

    pub fn sensitive_values(&self) -> &[String] {
        &self.sensitive_values
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn require_flag(name: &str) -> Result<(), String> {
    if env::var(name).as_deref() == Ok("1") {
        Ok(())
    } else {
        Err(format!("{name}=1 is required"))
    }
}

fn optional_flag(name: &str) -> Result<bool, String> {
    match env::var(name) {
        Ok(value) if value == "1" => Ok(true),
        Ok(value) if value == "0" || value.is_empty() => Ok(false),
        Ok(_) => Err(format!("{name} must be 0 or 1")),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(_) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn parse_providers(value: &str) -> Result<Vec<String>, String> {
    if value == "all" {
        return Ok(NETWORK_PROVIDERS.iter().map(ToString::to_string).collect());
    }
    let providers = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if providers.is_empty()
        || providers.len() > NETWORK_PROVIDERS.len()
        || providers
            .iter()
            .any(|provider| !NETWORK_PROVIDERS.contains(&provider.as_str()))
    {
        return Err(
            "NIB_LIVE_PROVIDER must be all or a comma-separated supported provider list"
                .to_string(),
        );
    }
    let mut unique = providers.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != providers.len() {
        return Err("NIB_LIVE_PROVIDER contains duplicates".to_string());
    }
    Ok(providers)
}

fn parse_usize(name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .ok_or_else(|| format!("{name} must be between {min} and {max}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn parse_u32(name: &str, default: u32, min: u32, max: u32) -> Result<u32, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .ok_or_else(|| format!("{name} must be between {min} and {max}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn parse_u64(name: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| (min..=max).contains(value))
            .ok_or_else(|| format!("{name} must be between {min} and {max}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(format!("{name} is not valid UTF-8")),
    }
}

pub(super) fn credential_env(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "google" => Some("GOOGLE_API_KEY"),
        "grok" => Some("XAI_API_KEY"),
        "meta" => Some("META_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parser_is_strict_and_deduplicated() {
        assert_eq!(parse_providers("openai,google").unwrap().len(), 2);
        assert!(parse_providers("openai,openai").is_err());
        assert!(parse_providers("mock").is_err());
        assert_eq!(parse_providers("all").unwrap().len(), 6);
    }

    #[test]
    fn mode_parser_distinguishes_catalog_from_paid_modes() {
        assert!(!LiveMode::parse("catalog")
            .unwrap()
            .makes_generation_requests());
        assert!(LiveMode::parse("canary")
            .unwrap()
            .makes_generation_requests());
        assert!(LiveMode::parse("full").unwrap().makes_generation_requests());
        assert!(LiveMode::parse("smoke").is_err());
    }
}
