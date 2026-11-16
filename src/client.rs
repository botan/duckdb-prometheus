use crate::{
    error::{PrometheusError, Result, escape_control_characters},
    model::{
        LabelSet, MetadataRow, SampleRow, parse_instant_response, parse_metadata_response,
        parse_range_response, parse_series_response, parse_string_response,
    },
};
use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use std::{fmt::Write as _, io::Read, time::Duration};
use url::Url;

// Keep error messages useful without buffering an untrusted error response. Raising
// this value directly increases the maximum diagnostic text retained per request.
const ERROR_PREVIEW_BYTES: usize = 1_024;
const ERROR_PREVIEW_READ_LIMIT: u64 = ERROR_PREVIEW_BYTES as u64;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    endpoint: Url,
    request_timeout: Option<Duration>,
}

impl ClientConfig {
    pub fn new(endpoint: &str, request_timeout: Option<Duration>) -> Result<Self> {
        Ok(Self {
            endpoint: normalize_endpoint(endpoint)?,
            request_timeout,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RangeQueryConfig {
    client: ClientConfig,
    query: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
}

impl RangeQueryConfig {
    pub fn new(
        client: ClientConfig,
        query: String,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Self> {
        validate_time_range(start, end)?;
        Ok(Self {
            client,
            query,
            start,
            end,
            step,
        })
    }
}

#[derive(Debug, Clone)]
pub struct InstantQueryConfig {
    client: ClientConfig,
    query: String,
    time: Option<DateTime<Utc>>,
}

impl InstantQueryConfig {
    pub fn new(client: ClientConfig, query: String, time: Option<DateTime<Utc>>) -> Self {
        Self {
            client,
            query,
            time,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    client: ClientConfig,
    matcher: Option<String>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
}

impl DiscoveryConfig {
    pub fn new(
        client: ClientConfig,
        matcher: Option<String>,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Self> {
        if let (Some(start), Some(end)) = (start, end) {
            validate_time_range(start, end)?;
        }
        Ok(Self {
            client,
            matcher,
            start,
            end,
        })
    }
}

pub fn fetch_range(config: &RangeQueryConfig) -> Result<Vec<SampleRow>> {
    let params = [
        ("query", config.query.clone()),
        ("start", format_timestamp(config.start)),
        ("end", format_timestamp(config.end)),
        ("step", duration_as_seconds(config.step)),
    ];
    let url = build_api_url(
        &config.client,
        "/api/v1/query_range",
        &["query", "start", "end", "step"],
        &params,
    );
    let body = fetch_body(&config.client, url)?;
    parse_range_response(&body)
}

pub fn fetch_instant(config: &InstantQueryConfig) -> Result<Vec<SampleRow>> {
    let mut params = vec![("query", config.query.clone())];
    if let Some(time) = config.time {
        params.push(("time", format_timestamp(time)));
    }
    let url = build_api_url(&config.client, "/api/v1/query", &["query", "time"], &params);
    let body = fetch_body(&config.client, url)?;
    parse_instant_response(&body)
}

pub fn fetch_labels(config: &DiscoveryConfig) -> Result<Vec<String>> {
    let params = discovery_parameters(config);
    let url = build_api_url(
        &config.client,
        "/api/v1/labels",
        &["match[]", "start", "end"],
        &params,
    );
    let body = fetch_body(&config.client, url)?;
    parse_string_response(&body)
}

pub fn fetch_label_values(config: &DiscoveryConfig, label: &str) -> Result<Vec<String>> {
    let params = discovery_parameters(config);
    let encoded_label = encode_label_name(label);
    let path = format!("/api/v1/label/{encoded_label}/values");
    let url = build_api_url(&config.client, &path, &["match[]", "start", "end"], &params);
    let body = fetch_body(&config.client, url)?;
    parse_string_response(&body)
}

pub fn fetch_series(config: &DiscoveryConfig) -> Result<Vec<LabelSet>> {
    let params = discovery_parameters(config);
    let url = build_api_url(
        &config.client,
        "/api/v1/series",
        &["match[]", "start", "end"],
        &params,
    );
    let body = fetch_body(&config.client, url)?;
    parse_series_response(&body)
}

pub fn fetch_metadata(config: &ClientConfig, metric: Option<&str>) -> Result<Vec<MetadataRow>> {
    let params = metric.map_or_else(Vec::new, |value| vec![("metric", value.to_string())]);
    let url = build_api_url(config, "/api/v1/metadata", &["metric"], &params);
    let body = fetch_body(config, url)?;
    parse_metadata_response(&body)
}

fn discovery_parameters(config: &DiscoveryConfig) -> Vec<(&'static str, String)> {
    let mut params = Vec::with_capacity(3);
    if let Some(matcher) = &config.matcher {
        params.push(("match[]", matcher.clone()));
    }
    if let Some(start) = config.start {
        params.push(("start", format_timestamp(start)));
    }
    if let Some(end) = config.end {
        params.push(("end", format_timestamp(end)));
    }
    params
}

fn fetch_body(config: &ClientConfig, url: Url) -> Result<Vec<u8>> {
    let client = Client::builder()
        .timeout(config.request_timeout)
        .user_agent(concat!("duckdb-prometheus/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(PrometheusError::BuildClient)?;

    let mut response = client.get(url).send().map_err(PrometheusError::Request)?;
    let status = response.status();
    if !status.is_success() {
        let preview = bounded_preview(&mut response)
            .map_err(|source| PrometheusError::ReadErrorResponse { status, source })?;
        return Err(PrometheusError::HttpStatus { status, preview });
    }

    response_body(&mut response).map_err(PrometheusError::ReadResponse)
}

fn normalize_endpoint(endpoint: &str) -> Result<Url> {
    let mut url = Url::parse(endpoint).map_err(PrometheusError::InvalidEndpoint)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(PrometheusError::UnsupportedEndpointScheme);
    }
    if url.cannot_be_a_base() || url.host_str().is_none() {
        return Err(PrometheusError::EndpointIsNotAbsolute);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PrometheusError::EndpointContainsCredentials);
    }
    if url.fragment().is_some() {
        return Err(PrometheusError::EndpointContainsFragment);
    }

    let path = url.path().trim_end_matches('/');
    let base_path = path
        .rfind("/api/v1/")
        .map_or(path, |api_index| &path[..api_index]);
    let base_path = base_path.strip_suffix("/api/v1").unwrap_or(base_path);
    let normalized = base_path.to_string();
    url.set_path(&normalized);
    Ok(url)
}

fn build_api_url(
    config: &ClientConfig,
    api_path: &str,
    reserved_parameters: &[&str],
    parameters: &[(&str, String)],
) -> Url {
    let mut url = config.endpoint.clone();
    let base_path = url.path().trim_end_matches('/');
    let full_path = format!("{base_path}{api_path}");
    url.set_path(&full_path);

    let retained_pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !reserved_parameters.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(retained_pairs)
        .extend_pairs(parameters.iter().map(|(key, value)| (*key, value.as_str())));
    url
}

fn validate_time_range(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<()> {
    if start > end {
        Err(PrometheusError::InvalidArgument(
            "start must not be after end".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    // Use i128 to cover timestamps outside the i64 nanosecond range.
    let nanos = i128::from(timestamp.timestamp()) * 1_000_000_000
        + i128::from(timestamp.timestamp_subsec_nanos());
    let sign = if nanos < 0 { "-" } else { "" };
    let magnitude = nanos.abs();
    let seconds = magnitude / 1_000_000_000;
    let fraction = magnitude % 1_000_000_000;
    if fraction == 0 {
        return format!("{sign}{seconds}");
    }

    let mut formatted = format!("{sign}{seconds}.{fraction:09}");
    while formatted.ends_with('0') {
        formatted.pop();
    }
    formatted
}

fn duration_as_seconds(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let micros = duration.subsec_micros();
    if micros == 0 {
        return seconds.to_string();
    }

    let mut fraction = format!("{micros:06}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{seconds}.{fraction}")
}

fn encode_label_name(label: &str) -> String {
    if label
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return label.to_string();
    }

    let mut encoded = String::from("U__");
    for character in label.chars() {
        if character.is_ascii_alphanumeric() || character == ':' {
            encoded.push(character);
        } else if character == '_' {
            encoded.push_str("__");
        } else {
            encoded.push('_');
            write!(&mut encoded, "{:x}", u32::from(character))
                .expect("writing to a String cannot fail");
            encoded.push('_');
        }
    }
    encoded
}

fn bounded_preview(reader: &mut impl Read) -> std::io::Result<String> {
    let (bytes, truncated) = read_limited(reader, ERROR_PREVIEW_READ_LIMIT)?;

    let decoded = String::from_utf8_lossy(&bytes);
    let mut preview = escape_control_characters(decoded.trim());
    if preview.is_empty() {
        preview.push_str("<empty>");
    }
    if truncated {
        preview.push('…');
    }
    Ok(preview)
}

fn response_body(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    reader.read_to_end(&mut body)?;
    Ok(body)
}

fn read_limited(reader: &mut impl Read, limit: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let read_limit = limit.saturating_add(1);
    let mut bytes = Vec::new();
    reader.take(read_limit).read_to_end(&mut bytes)?;
    let truncated = u64::try_from(bytes.len()).is_ok_and(|length| length > limit);
    if truncated {
        let retained_length = usize::try_from(limit).unwrap_or(usize::MAX);
        bytes.truncate(retained_length);
    }
    Ok((bytes, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use std::{collections::BTreeMap, io::Cursor};

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("test timestamp is in range")
    }

    fn client(endpoint: &str) -> ClientConfig {
        ClientConfig::new(endpoint, None).expect("valid endpoint")
    }

    fn pairs(url: &Url) -> BTreeMap<String, String> {
        url.query_pairs().into_owned().collect()
    }

    #[test]
    fn formats_epoch_seconds_without_losing_fractional_precision() {
        let cases = [
            (0, 0, "0"),
            (1, 0, "1"),
            (-1, 0, "-1"),
            (1_784_541_600, 123_456_789, "1784541600.123456789"),
            (0, 1, "0.000000001"),
            (0, 999_999_999, "0.999999999"),
            (-1, 500_000_000, "-0.5"),
            (-1, 999_999_999, "-0.000000001"),
            (-2, 999_999_999, "-1.000000001"),
            (32_503_680_000, 1, "32503680000.000000001"),
            (-32_503_680_000, 1, "-32503679999.999999999"),
        ];
        for (seconds, nanos, expected) in cases {
            let timestamp = DateTime::from_timestamp(seconds, nanos).expect("valid timestamp");
            assert_eq!(format_timestamp(timestamp), expected, "{timestamp}");
        }
    }

    #[test]
    fn builds_range_and_instant_urls_from_shared_endpoint() {
        let range = RangeQueryConfig::new(
            client("http://localhost:9090"),
            r#"up{job="api server"}"#.to_string(),
            timestamp(1_784_541_600),
            timestamp(1_784_545_200),
            Duration::from_secs(60),
        )
        .expect("valid range");
        let range_url = build_api_url(
            &range.client,
            "/api/v1/query_range",
            &["query", "start", "end", "step"],
            &[
                ("query", range.query.clone()),
                ("start", format_timestamp(range.start)),
                ("end", format_timestamp(range.end)),
                ("step", duration_as_seconds(range.step)),
            ],
        );
        assert_eq!(range_url.path(), "/api/v1/query_range");
        assert_eq!(pairs(&range_url)["query"], r#"up{job="api server"}"#);
        assert_eq!(pairs(&range_url)["start"], "1784541600");
        assert_eq!(pairs(&range_url)["end"], "1784545200");
        assert_eq!(pairs(&range_url)["step"], "60");

        let instant = InstantQueryConfig::new(
            client("http://localhost:9090"),
            "up".to_string(),
            Some(timestamp(1_784_541_600)),
        );
        let instant_url = build_api_url(
            &instant.client,
            "/api/v1/query",
            &["query", "time"],
            &[
                ("query", instant.query),
                ("time", format_timestamp(instant.time.expect("time is set"))),
            ],
        );
        assert_eq!(instant_url.path(), "/api/v1/query");
        assert_eq!(pairs(&instant_url)["time"], "1784541600");
    }

    #[test]
    fn preserves_proxy_path_and_non_api_query_parameters() {
        let config =
            client("https://example.test/prometheus/api/v1/query_range/?region=eu&query=old");
        let url = build_api_url(
            &config,
            "/api/v1/labels",
            &["match[]", "start", "end"],
            &[("match[]", "up".to_string())],
        );
        assert_eq!(url.path(), "/prometheus/api/v1/labels");
        assert_eq!(pairs(&url)["region"], "eu");
        assert_eq!(pairs(&url)["query"], "old");
        assert_eq!(pairs(&url)["match[]"], "up");
    }

    #[test]
    fn replaces_operation_parameters_and_removes_omitted_time() {
        let config = client("https://example.test/?query=old&time=1&region=eu");
        let url = build_api_url(
            &config,
            "/api/v1/query",
            &["query", "time"],
            &[("query", "new".to_string())],
        );
        assert_eq!(pairs(&url)["query"], "new");
        assert_eq!(pairs(&url)["region"], "eu");
        assert!(!pairs(&url).contains_key("time"));
    }

    #[test]
    fn encodes_utf8_label_names_for_the_values_endpoint() {
        assert_eq!(encode_label_name("http_status"), "http_status");
        assert_eq!(encode_label_name("http.status"), "U__http_2e_status");
        assert_eq!(encode_label_name("a_b:c"), "U__a__b:c");
    }

    #[test]
    fn rejects_unsupported_or_ambiguous_endpoints() {
        assert!(matches!(
            ClientConfig::new("ftp://example.test", None),
            Err(PrometheusError::UnsupportedEndpointScheme)
        ));
        assert!(matches!(
            ClientConfig::new("http://user:pass@example.test", None),
            Err(PrometheusError::EndpointContainsCredentials)
        ));
        assert!(matches!(
            ClientConfig::new("http://example.test/#fragment", None),
            Err(PrometheusError::EndpointContainsFragment)
        ));
        assert!(matches!(
            ClientConfig::new("/relative", None),
            Err(PrometheusError::InvalidEndpoint(_))
        ));
    }

    #[test]
    fn rejects_reversed_ranges() {
        let result = RangeQueryConfig::new(
            client("http://localhost:9090"),
            "up".to_string(),
            timestamp(2),
            timestamp(1),
            Duration::from_secs(60),
        );
        assert!(matches!(result, Err(PrometheusError::InvalidArgument(_))));
    }

    #[test]
    fn formats_subsecond_steps_without_trailing_zeroes() {
        assert_eq!(duration_as_seconds(Duration::from_millis(1_500)), "1.5");
        assert_eq!(duration_as_seconds(Duration::from_micros(1)), "0.000001");
    }

    #[test]
    fn truncates_error_preview_and_escapes_control_characters() {
        let input = format!("first\n{}", "x".repeat(ERROR_PREVIEW_BYTES));
        let preview = bounded_preview(&mut Cursor::new(input.as_bytes())).expect("preview");
        assert!(preview.starts_with(r"first\n"));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.len(), ERROR_PREVIEW_BYTES + 4);
    }

    #[test]
    fn preserves_short_and_empty_error_previews() {
        let preview = bounded_preview(&mut Cursor::new(b"  unavailable  ")).expect("preview");
        assert_eq!(preview, "unavailable");
        let empty = bounded_preview(&mut Cursor::new(b"\n\t")).expect("empty preview");
        assert_eq!(empty, "<empty>");
    }

    #[test]
    fn status_code_is_available_for_http_errors() {
        let error = PrometheusError::HttpStatus {
            status: StatusCode::BAD_GATEWAY,
            preview: "upstream".to_string(),
        };
        assert!(error.to_string().contains("502 Bad Gateway"));
    }
}
