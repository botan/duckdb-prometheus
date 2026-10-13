use crate::{
    client::ClientConfig,
    error::{PrometheusError, Result},
};
use chrono::{DateTime, NaiveDateTime, Utc};
use duckdb::{
    core::{LogicalTypeHandle, LogicalTypeId},
    vtab::BindInfo,
};
use std::time::Duration;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

pub fn connection_named_parameters() -> Vec<(String, LogicalTypeHandle)> {
    vec![
        (
            "endpoint".to_string(),
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        ),
        (
            "request_timeout".to_string(),
            LogicalTypeHandle::from(LogicalTypeId::Interval),
        ),
    ]
}

pub fn client_config(bind: &BindInfo) -> Result<ClientConfig> {
    let endpoint = required_named_string(bind, "endpoint")?;
    let request_timeout = named_interval(bind, "request_timeout")?;
    ClientConfig::new(&endpoint, request_timeout)
}

pub fn required_parameter(bind: &BindInfo, index: u64, name: &str) -> Result<String> {
    let value = bind.get_parameter(index);
    if value.is_null() {
        return Err(invalid_argument(format!("{name} must not be NULL")));
    }
    let value = value.to_string();
    if value.is_empty() {
        return Err(invalid_argument(format!("{name} must not be empty")));
    }
    Ok(value)
}

pub fn required_string_list_parameter(
    bind: &BindInfo,
    index: u64,
    name: &str,
) -> Result<Vec<String>> {
    let value = bind.get_parameter(index);
    let values = string_list(&value, name)?;
    if values.is_empty() {
        Err(invalid_argument(format!("{name} must not be empty")))
    } else {
        Ok(values)
    }
}

pub fn required_named_string(bind: &BindInfo, name: &str) -> Result<String> {
    optional_named_string(bind, name)?
        .ok_or_else(|| invalid_argument(format!("{name} must be provided")))
}

pub fn optional_named_string(bind: &BindInfo, name: &str) -> Result<Option<String>> {
    match bind.get_named_parameter(name) {
        Some(value) if value.is_null() => Err(invalid_argument(format!("{name} must not be NULL"))),
        Some(value) => {
            let value = value.to_string();
            if value.is_empty() {
                Err(invalid_argument(format!("{name} must not be empty")))
            } else {
                Ok(Some(value))
            }
        }
        None => Ok(None),
    }
}

pub fn required_timestamp(bind: &BindInfo, index: u64, name: &str) -> Result<DateTime<Utc>> {
    let value = bind.get_parameter(index);
    parse_timestamp_value(&value, name)
}

pub fn optional_named_timestamp(bind: &BindInfo, name: &str) -> Result<Option<DateTime<Utc>>> {
    match bind.get_named_parameter(name) {
        Some(value) => parse_timestamp_value(&value, name).map(Some),
        None => Ok(None),
    }
}

fn parse_timestamp_value(value: &duckdb::vtab::Value, name: &str) -> Result<DateTime<Utc>> {
    if value.is_null() {
        return Err(invalid_argument(format!("{name} must not be NULL")));
    }

    match value.logical_type_id() {
        LogicalTypeId::TimestampTZ => {
            DateTime::parse_from_str(&value.to_string(), "%Y-%m-%d %H:%M:%S%.f%#z")
                .map(|timestamp| timestamp.with_timezone(&Utc))
                .map_err(|_| invalid_argument(format!("{name} must be a finite TIMESTAMPTZ value")))
        }
        LogicalTypeId::Timestamp => parse_naive_timestamp(&value.to_string())
            .ok_or_else(|| invalid_argument(format!("{name} must be a finite TIMESTAMP value"))),
        LogicalTypeId::Varchar => parse_text_timestamp(&value.to_string()).ok_or_else(|| {
            invalid_argument(format!(
                "{name} must use YYYY-MM-DD HH:MM:SS[.ffffff][offset] format"
            ))
        }),
        _ => Err(invalid_argument(format!(
            "{name} must be VARCHAR, TIMESTAMP, or TIMESTAMPTZ"
        ))),
    }
}

fn parse_text_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f%#z"))
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .ok()
        .or_else(|| parse_naive_timestamp(value))
}

fn parse_naive_timestamp(value: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
        .map(|timestamp| timestamp.and_utc())
        .ok()
}

pub fn named_interval(bind: &BindInfo, name: &str) -> Result<Option<Duration>> {
    match bind.get_named_parameter(name) {
        Some(value) if value.is_null() => Err(invalid_argument(format!("{name} must not be NULL"))),
        // vtab::Value has no typed interval getter; its Display implementation
        // delegates to duckdb_get_varchar. Isolate parsing of that representation.
        Some(value) => parse_interval(&value.to_string(), name).map(Some),
        None => Ok(None),
    }
}

pub fn named_string_list(bind: &BindInfo, name: &str) -> Result<Vec<String>> {
    let Some(value) = bind.get_named_parameter(name) else {
        return Ok(Vec::new());
    };
    string_list(&value, name)
}

fn string_list(value: &duckdb::vtab::Value, name: &str) -> Result<Vec<String>> {
    if value.is_null() {
        return Err(invalid_argument(format!("{name} must not be NULL")));
    }

    let values = value
        .to_list()
        .ok_or_else(|| invalid_argument(format!("{name} must be a LIST of VARCHAR values")))?;
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            if value.is_null() {
                return Err(invalid_argument(format!(
                    "{name}[{index}] must not be NULL"
                )));
            }
            let value = value.to_string();
            if value.is_empty() {
                Err(invalid_argument(format!(
                    "{name}[{index}] must not be empty"
                )))
            } else {
                Ok(value)
            }
        })
        .collect()
}

fn parse_interval(value: &str, name: &str) -> Result<Duration> {
    if value.contains("month") || value.contains("year") {
        return Err(invalid_argument(format!(
            "{name} must not contain months or years"
        )));
    }

    let parts: Vec<&str> = value.split_whitespace().collect();
    let (days, time) = match parts.as_slice() {
        [time] => (0_i64, *time),
        [days, unit] if matches!(*unit, "day" | "days") => {
            (parse_interval_days(days, name)?, "00:00:00")
        }
        [days, unit, time] if matches!(*unit, "day" | "days") => {
            (parse_interval_days(days, name)?, *time)
        }
        _ => return Err(unsupported_interval(name)),
    };

    let day_micros = days
        .checked_mul(MICROS_PER_DAY)
        .ok_or_else(|| unsupported_interval(name))?;
    let time_micros = parse_interval_time(time, name)?;
    let total_micros = day_micros
        .checked_add(time_micros)
        .ok_or_else(|| unsupported_interval(name))?;
    if total_micros <= 0 {
        return Err(invalid_argument(format!(
            "{name} must be greater than zero"
        )));
    }

    let total_micros = u64::try_from(total_micros).map_err(|_| unsupported_interval(name))?;
    Ok(Duration::from_micros(total_micros))
}

fn parse_interval_days(value: &str, name: &str) -> Result<i64> {
    value.parse().map_err(|_| unsupported_interval(name))
}

fn parse_interval_time(value: &str, name: &str) -> Result<i64> {
    let (sign, unsigned) = value
        .strip_prefix('-')
        .map_or((1_i64, value), |value| (-1_i64, value));
    let mut components = unsigned.split(':');
    let hours = parse_u64(components.next(), name)?;
    let minutes = parse_u64(components.next(), name)?;
    let seconds = components
        .next()
        .ok_or_else(|| unsupported_interval(name))?;
    if components.next().is_some() || minutes >= 60 {
        return Err(unsupported_interval(name));
    }

    let (seconds, fractional_micros) = parse_interval_seconds(seconds, name)?;
    if seconds >= 60 {
        return Err(unsupported_interval(name));
    }

    let whole_seconds = hours
        .checked_mul(3_600)
        .and_then(|value| value.checked_add(minutes * 60))
        .and_then(|value| value.checked_add(seconds))
        .ok_or_else(|| unsupported_interval(name))?;
    let micros = whole_seconds
        .checked_mul(u64::try_from(MICROS_PER_SECOND).expect("constant fits in u64"))
        .and_then(|value| value.checked_add(fractional_micros))
        .ok_or_else(|| unsupported_interval(name))?;
    let micros = i64::try_from(micros).map_err(|_| unsupported_interval(name))?;
    micros
        .checked_mul(sign)
        .ok_or_else(|| unsupported_interval(name))
}

fn parse_interval_seconds(value: &str, name: &str) -> Result<(u64, u64)> {
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
    let seconds = seconds.parse().map_err(|_| unsupported_interval(name))?;
    if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported_interval(name));
    }

    let fractional_micros = if fraction.is_empty() {
        0
    } else {
        let parsed: u64 = fraction.parse().map_err(|_| unsupported_interval(name))?;
        parsed
            * 10_u64.pow(u32::try_from(6 - fraction.len()).expect("fraction length is at most six"))
    };
    Ok((seconds, fractional_micros))
}

fn parse_u64(value: Option<&str>, name: &str) -> Result<u64> {
    value
        .ok_or_else(|| unsupported_interval(name))?
        .parse()
        .map_err(|_| unsupported_interval(name))
}

fn unsupported_interval(name: &str) -> PrometheusError {
    invalid_argument(format!("{name} is outside the supported interval range"))
}

pub fn invalid_argument(message: impl Into<String>) -> PrometheusError {
    PrometheusError::InvalidArgument(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_duckdb_intervals() {
        assert_eq!(
            parse_interval("00:01:00", "step").expect("valid interval"),
            Duration::from_secs(60)
        );
        assert_eq!(
            parse_interval("1 day 00:00:00.000001", "step").expect("valid interval"),
            Duration::from_micros(86_400_000_001)
        );
        assert_eq!(
            parse_interval("48:00:00", "step").expect("valid interval"),
            Duration::from_secs(172_800)
        );
    }

    #[test]
    fn rejects_calendar_non_positive_and_unsupported_intervals() {
        assert_eq!(
            parse_interval("1 month", "step")
                .expect_err("calendar interval must fail")
                .to_string(),
            "step must not contain months or years"
        );
        assert_eq!(
            parse_interval("00:00:00", "step")
                .expect_err("zero interval must fail")
                .to_string(),
            "step must be greater than zero"
        );
        parse_interval("-1 day 00:00:01", "step").expect_err("negative interval must fail");
        parse_interval("not-an-interval", "step").expect_err("invalid interval must fail");
    }

    #[test]
    fn parses_text_timestamps_with_sql_rfc3339_and_utc_default_formats() {
        let expected = DateTime::parse_from_rfc3339("2026-07-20T10:00:00Z")
            .expect("valid expected timestamp")
            .with_timezone(&Utc);
        assert_eq!(parse_text_timestamp("2026-07-20T10:00:00Z"), Some(expected));
        assert_eq!(
            parse_text_timestamp("2026-07-20 11:00:00+01"),
            Some(expected)
        );
        assert_eq!(parse_text_timestamp("2026-07-20 10:00:00"), Some(expected));
        assert_eq!(parse_text_timestamp("not-a-timestamp"), None);
    }
}
