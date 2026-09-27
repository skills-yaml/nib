//! Bounded HTTP fetch, search parsing, and HTML sanitization for core tools.

use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::time::timeout;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HTTP_RESPONSE_BYTES: usize = 1_048_576;
const MAX_HTTP_REDIRECTS: usize = 5;

pub(crate) struct HttpDocument {
    pub(crate) final_url: reqwest::Url,
    pub(crate) content_type: Option<String>,
    pub(crate) body: Vec<u8>,
}

pub(crate) async fn fetch_bounded(url: reqwest::Url) -> Result<HttpDocument, String> {
    fetch_bounded_with_resolver(url, &SystemHostResolver).await
}

#[async_trait::async_trait]
pub(crate) trait HostResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String>;
}

pub(crate) struct SystemHostResolver;

#[async_trait::async_trait]
impl HostResolver for SystemHostResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let host = host.to_string();
        tokio::task::spawn_blocking(move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect())
                .map_err(|error| format!("DNS resolution failed for {host}: {error}"))
        })
        .await
        .map_err(|error| format!("DNS resolver task failed: {error}"))?
    }
}

pub(crate) struct ResolvedDestination {
    pub(crate) host: String,
    pub(crate) addresses: Vec<SocketAddr>,
}

pub(crate) async fn resolve_public_destination<R: HostResolver + ?Sized>(
    url: &reqwest::Url,
    resolver: &R,
) -> Result<ResolvedDestination, String> {
    let host = url.host_str().ok_or("URL must include a host")?;
    let host = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or("URL has no known destination port")?;
    let mut addresses = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, port)]
    } else {
        resolver.resolve(&host, port).await?
    };
    if addresses.is_empty() {
        return Err(format!("DNS resolution returned no addresses for {host}"));
    }
    for address in &mut addresses {
        address.set_port(port);
        if ip_is_non_public(address.ip()) {
            return Err(format!(
                "resolved URL host {host} to non-public address {}",
                address.ip()
            ));
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(ResolvedDestination { host, addresses })
}

pub(crate) async fn fetch_bounded_with_resolver<R: HostResolver + ?Sized>(
    url: reqwest::Url,
    resolver: &R,
) -> Result<HttpDocument, String> {
    timeout(HTTP_TIMEOUT, async move {
        let mut current_url = validate_http_url(url.as_str())?;
        for redirect_count in 0..=MAX_HTTP_REDIRECTS {
            let destination = resolve_public_destination(&current_url, resolver).await?;
            let client = reqwest::Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(HTTP_TIMEOUT)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .resolve_to_addrs(&destination.host, &destination.addresses)
                .user_agent("nib/0.1 (+https://github.com/skills-yaml/nib)")
                .build()
                .map_err(|error| format!("failed to create HTTP client: {error}"))?;
            let response = client
                .get(current_url.clone())
                .header(
                    "accept",
                    "text/html, application/xhtml+xml, text/markdown, text/plain;q=0.9",
                )
                .send()
                .await
                .map_err(|error| format!("HTTP request failed: {error}"))?;
            let status = response.status();
            if status.is_redirection() {
                if redirect_count >= MAX_HTTP_REDIRECTS {
                    return Err(format!(
                        "HTTP redirect limit exceeded ({MAX_HTTP_REDIRECTS})"
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .ok_or_else(|| format!("HTTP redirect status {status} omitted Location"))?
                    .to_str()
                    .map_err(|error| format!("HTTP redirect Location is invalid: {error}"))?;
                current_url = validated_redirect_target(&current_url, location)?;
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP request returned status {status}"));
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_HTTP_RESPONSE_BYTES as u64)
            {
                return Err(format!(
                    "HTTP response exceeds {} byte limit",
                    MAX_HTTP_RESPONSE_BYTES
                ));
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|error| format!("failed to read HTTP response: {error}"))?;
                if body.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
                    return Err(format!(
                        "HTTP response exceeds {} byte limit",
                        MAX_HTTP_RESPONSE_BYTES
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(HttpDocument {
                final_url: current_url,
                content_type,
                body,
            });
        }
        Err(format!(
            "HTTP redirect limit exceeded ({MAX_HTTP_REDIRECTS})"
        ))
    })
    .await
    .map_err(|_| format!("HTTP request timed out after {}s", HTTP_TIMEOUT.as_secs()))?
}

pub(crate) fn validated_redirect_target(
    current_url: &reqwest::Url,
    location: &str,
) -> Result<reqwest::Url, String> {
    let redirect_url = current_url
        .join(location)
        .map_err(|error| format!("invalid HTTP redirect target: {error}"))?;
    validate_http_url(redirect_url.as_str())
        .map_err(|error| format!("HTTP redirect target rejected: {error}"))
}

pub(crate) fn validate_http_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL scheme must be http or https".to_string());
    }
    let host = url.host_str().ok_or("URL must include a host")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are not allowed".to_string());
    }
    let normalized_host = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if normalized_host == "localhost" || normalized_host.ends_with(".localhost") {
        return Err("URL host is not allowed: localhost".to_string());
    }
    if let Ok(address) = normalized_host.parse::<IpAddr>() {
        if ip_is_non_public(address) {
            return Err(format!("URL host is not publicly routable: {address}"));
        }
    }
    Ok(url)
}

pub(crate) fn ip_is_non_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_is_non_public(address),
        IpAddr::V6(address) => ipv6_is_non_public(address),
    }
}

pub(crate) fn ipv4_is_non_public(address: Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    first == 0
        || first == 10
        || first == 127
        || (first == 100 && (64..=127).contains(&second))
        || (first == 169 && second == 254)
        || (first == 172 && (16..=31).contains(&second))
        || (first == 192 && second == 0 && third == 0)
        || (first == 192 && second == 0 && third == 2)
        || (first == 192 && second == 88 && third == 99)
        || (first == 192 && second == 168)
        || (first == 198 && matches!(second, 18 | 19))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 224
}

pub(crate) fn ipv6_is_non_public(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    let unique_local = octets[0] & 0xfe == 0xfc;
    let link_local = octets[0] == 0xfe && octets[1] & 0xc0 == 0x80;
    let site_local = octets[0] == 0xfe && octets[1] & 0xc0 == 0xc0;
    let documentation = octets[..4] == [0x20, 0x01, 0x0d, 0xb8];
    let discard_only = octets[..8] == [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let transition_protocol = octets[..4] == [0x20, 0x01, 0x00, 0x00]
        || octets[..2] == [0x20, 0x02]
        || octets[..4] == [0x20, 0x01, 0x00, 0x10]
        || octets[..4] == [0x20, 0x01, 0x00, 0x20];
    let nat64_ipv4 = if octets[..12]
        == [
            0x00, 0x64, 0xff, 0x9b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]
        || octets[..6] == [0x00, 0x64, 0xff, 0x9b, 0x00, 0x01]
    {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    };
    let embedded_ipv4 = if octets[..10] == [0; 10]
        && (octets[10..12] == [0xff, 0xff] || octets[10..12] == [0, 0])
    {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    };
    address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || unique_local
        || link_local
        || site_local
        || documentation
        || discard_only
        || transition_protocol
        || nat64_ipv4.is_some_and(ipv4_is_non_public)
        || embedded_ipv4.is_some_and(ipv4_is_non_public)
}

pub(crate) fn ensure_textual_content_type(content_type: Option<&str>) -> Result<(), String> {
    let Some(content_type) = content_type else {
        return Ok(());
    };
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type.starts_with("text/") || media_type == "application/xhtml+xml" {
        Ok(())
    } else {
        Err(format!("unsupported URL content type: {media_type}"))
    }
}

pub(crate) fn parse_search_results(html: &str, max_results: usize) -> Vec<Value> {
    let snippets: Vec<_> = SEARCH_SNIPPET_RE
        .captures_iter(html)
        .filter_map(|captures| captures.name("body"))
        .map(|body| html_fragment_text(body.as_str()))
        .collect();
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();
    for captures in HTML_ANCHOR_RE.captures_iter(html) {
        let Some(attributes) = captures.name("attrs").map(|value| value.as_str()) else {
            continue;
        };
        if !attribute_has_class(attributes, "result__a") {
            continue;
        }
        let Some(href) = quoted_attribute(attributes, &HREF_ATTRIBUTE_RE) else {
            continue;
        };
        let Some(url) = normalize_search_result_url(href) else {
            continue;
        };
        if !seen_urls.insert(url.clone()) {
            continue;
        }
        let title = captures
            .name("body")
            .map(|body| html_fragment_text(body.as_str()))
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let snippet = snippets.get(results.len()).cloned().unwrap_or_default();
        results.push(json!({"title": title, "url": url, "snippet": snippet}));
        if results.len() >= max_results {
            break;
        }
    }
    results
}

pub(crate) fn normalize_search_result_url(raw: &str) -> Option<String> {
    let decoded = decode_html_entities(raw);
    let absolute = if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded
    };
    let url = validate_http_url(&absolute).ok()?;
    if url
        .host_str()
        .is_some_and(|host| host.ends_with("duckduckgo.com"))
    {
        if let Some((_, target)) = url.query_pairs().find(|(key, _)| key == "uddg") {
            return validate_http_url(&target).ok().map(|url| url.to_string());
        }
    }
    Some(url.to_string())
}

pub(crate) fn html_to_safe_markdown(html: &str, base_url: &reqwest::Url) -> String {
    let without_dangerous = strip_dangerous_html_blocks(html);
    let without_comments = HTML_COMMENT_RE.replace_all(&without_dangerous, "");
    let with_headings =
        HTML_HEADING_RE.replace_all(&without_comments, |captures: &regex::Captures| {
            let level = captures
                .get(1)
                .and_then(|value| value.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            let text = captures
                .get(2)
                .map(|value| html_fragment_text(value.as_str()))
                .unwrap_or_default();
            format!("\n{} {}\n", "#".repeat(level), text)
        });
    let with_links = HTML_ANCHOR_RE.replace_all(&with_headings, |captures: &regex::Captures| {
        let text = captures
            .name("body")
            .map(|value| html_fragment_text(value.as_str()))
            .unwrap_or_default();
        let href = captures
            .name("attrs")
            .and_then(|attrs| quoted_attribute(attrs.as_str(), &HREF_ATTRIBUTE_RE));
        match href.and_then(|href| safe_link_target(href, base_url)) {
            Some(target) if !text.is_empty() => {
                format!("[{}]({target})", escape_markdown_text(&text))
            }
            _ => text,
        }
    });
    let with_list_items =
        HTML_LIST_ITEM_RE.replace_all(&with_links, |captures: &regex::Captures| {
            let text = captures
                .get(1)
                .map(|value| html_fragment_text(value.as_str()))
                .unwrap_or_default();
            format!("\n- {text}\n")
        });
    let with_breaks = HTML_BREAK_RE.replace_all(&with_list_items, "\n");
    let with_blocks = HTML_BLOCK_RE.replace_all(&with_breaks, "\n");
    let without_tags = HTML_TAG_RE.replace_all(&with_blocks, "");
    let normalized =
        normalize_markdown_whitespace(&sanitize_text(&decode_html_entities(&without_tags)));
    normalized.replace('<', "&lt;").replace('>', "&gt;")
}

pub(crate) fn strip_dangerous_html_blocks(input: &str) -> String {
    DANGEROUS_HTML_RE.replace_all(input, "").into_owned()
}

pub(crate) fn safe_link_target(raw: &str, base_url: &reqwest::Url) -> Option<String> {
    let decoded = decode_html_entities(raw);
    let target = reqwest::Url::parse(&decoded)
        .or_else(|_| base_url.join(&decoded))
        .ok()?;
    validate_http_url(target.as_str())
        .ok()
        .map(|url| url.to_string())
}

pub(crate) fn extract_page_title(html: &str) -> Option<String> {
    HTML_TITLE_RE
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|title| html_fragment_text(title.as_str()))
        .filter(|title| !title.is_empty())
}

pub(crate) fn html_fragment_text(fragment: &str) -> String {
    let without_tags = HTML_TAG_RE.replace_all(fragment, " ");
    normalize_inline_whitespace(&sanitize_text(&decode_html_entities(&without_tags)))
}

pub(crate) fn attribute_has_class(attributes: &str, expected: &str) -> bool {
    quoted_attribute(attributes, &CLASS_ATTRIBUTE_RE).is_some_and(|classes| {
        classes
            .split_ascii_whitespace()
            .any(|class_name| class_name == expected)
    })
}

pub(crate) fn quoted_attribute<'a>(attributes: &'a str, expression: &Regex) -> Option<&'a str> {
    let captures = expression.captures(attributes)?;
    captures
        .get(1)
        .or_else(|| captures.get(2))
        .map(|value| value.as_str())
}

pub(crate) fn decode_html_entities(input: &str) -> String {
    HTML_ENTITY_RE
        .replace_all(input, |captures: &regex::Captures| {
            let entity = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            match entity {
                "amp" => "&".to_string(),
                "lt" => "<".to_string(),
                "gt" => ">".to_string(),
                "quot" => "\"".to_string(),
                "apos" => "'".to_string(),
                "nbsp" => " ".to_string(),
                numeric if numeric.starts_with("#x") || numeric.starts_with("#X") => {
                    u32::from_str_radix(&numeric[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|character| character.to_string())
                        .unwrap_or_default()
                }
                numeric if numeric.starts_with('#') => numeric[1..]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|character| character.to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            }
        })
        .into_owned()
}

pub(crate) fn sanitize_text(input: &str) -> String {
    input
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

pub(crate) fn normalize_inline_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn normalize_markdown_whitespace(input: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = true;
    for raw_line in input.lines() {
        let line = normalize_inline_whitespace(raw_line);
        if line.is_empty() {
            if !previous_blank {
                lines.push(String::new());
            }
            previous_blank = true;
        } else {
            lines.push(line);
            previous_blank = false;
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

pub(crate) fn escape_markdown_text(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

pub(crate) fn truncate_chars(input: &str, limit: usize) -> (String, bool) {
    if input.chars().count() <= limit {
        return (input.to_string(), false);
    }
    (input.chars().take(limit).collect(), true)
}

pub(crate) fn required_nonempty_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing or empty {key}"))
}

pub(crate) fn required_identifier<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    let value = required_nonempty_string(args, key)?;
    if value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!("invalid {key}"));
    }
    Ok(value)
}

pub(crate) fn bounded_usize_arg(
    args: &Value,
    key: &str,
    default: usize,
    minimum: usize,
    maximum: usize,
) -> Result<usize, String> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("{key} must be an unsigned integer"))?,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{key} must be between {minimum} and {maximum}"));
    }
    Ok(value)
}

pub(crate) fn bounded_u64_arg(
    args: &Value,
    key: &str,
    default: Option<u64>,
    minimum: u64,
    maximum: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an unsigned integer"))?,
        None => default.ok_or_else(|| format!("missing {key}"))?,
    };
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{key} must be between {minimum} and {maximum}"));
    }
    Ok(value)
}

static HTML_ANCHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<a\b(?P<attrs>[^>]*)>(?P<body>.*?)</a\s*>"#).expect("anchor regex")
});
static SEARCH_SNIPPET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<(?:a|div)\b(?P<attrs>[^>]*\bclass\s*=\s*(?:\"[^\"]*result__snippet[^\"]*\"|'[^']*result__snippet[^']*')[^>]*)>(?P<body>.*?)</(?:a|div)\s*>"#,
    )
    .expect("search snippet regex")
});
static CLASS_ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bclass\s*=\s*(?:\"([^\"]*)\"|'([^']*)')"#).expect("class regex")
});
static HREF_ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\bhref\s*=\s*(?:\"([^\"]*)\"|'([^']*)')"#).expect("href regex")
});
static HTML_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<!--.*?-->").expect("comment regex"));
static DANGEROUS_HTML_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>|<noscript\b[^>]*>.*?</noscript\s*>|<iframe\b[^>]*>.*?</iframe\s*>|<object\b[^>]*>.*?</object\s*>|<svg\b[^>]*>.*?</svg\s*>|<template\b[^>]*>.*?</template\s*>",
    )
    .expect("dangerous HTML regex")
});
static HTML_HEADING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]\s*>").expect("heading regex")
});
static HTML_LIST_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<li\b[^>]*>(.*?)</li\s*>").expect("list item regex"));
static HTML_BREAK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<br\b[^>]*>").expect("break regex"));
static HTML_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)</?(?:article|aside|blockquote|div|dl|dt|dd|footer|header|hr|main|nav|ol|p|pre|section|table|tbody|td|th|thead|tr|ul)\b[^>]*>",
    )
    .expect("block regex")
});
static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").expect("tag regex"));
static HTML_TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title\s*>").expect("title regex"));
static HTML_ENTITY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#(?:x|X)[0-9a-fA-F]+|#[0-9]+|amp|lt|gt|quot|apos|nbsp);").expect("entity regex")
});
