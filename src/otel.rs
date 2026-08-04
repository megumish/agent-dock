use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OTelProvider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxiliaryFact {
    pub provider: OTelProvider,
    pub external_session_id: String,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub estimated_cost_usd_micros: Option<u64>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OtelParseError {
    #[error("OTLP input is too large")]
    InputTooLarge,
    #[error("invalid OTLP logs payload")]
    InvalidPayload,
    #[error("invalid allowlisted OTLP event")]
    InvalidEvent,
}

/// Parses an OTLP/HTTP JSON `ExportLogsServiceRequest` without retaining
/// attributes outside the provider-specific allowlist.
pub fn parse_otlp_logs(
    provider: OTelProvider,
    input: &[u8],
) -> Result<Vec<AuxiliaryFact>, OtelParseError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(OtelParseError::InputTooLarge);
    }
    let request: ExportLogsRequest =
        serde_json::from_slice(input).map_err(|_| OtelParseError::InvalidPayload)?;
    let mut facts = Vec::new();
    for resource_logs in request.resource_logs {
        for scope_logs in resource_logs.scope_logs {
            for record in scope_logs.log_records {
                if let Some(fact) = parse_record(provider, record)? {
                    facts.push(fact);
                }
            }
        }
    }
    Ok(facts)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportLogsRequest {
    #[serde(default)]
    resource_logs: Vec<ResourceLogs>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceLogs {
    #[serde(default)]
    scope_logs: Vec<ScopeLogs>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScopeLogs {
    #[serde(default)]
    log_records: Vec<LogRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogRecord {
    time_unix_nano: Option<Value>,
    #[serde(default)]
    attributes: Vec<Attribute>,
}

#[derive(Debug, Deserialize)]
struct Attribute {
    key: String,
    value: Value,
}

fn parse_record(
    provider: OTelProvider,
    record: LogRecord,
) -> Result<Option<AuxiliaryFact>, OtelParseError> {
    let allowlist = match provider {
        OTelProvider::Claude => CLAUDE_ATTRIBUTES,
        OTelProvider::Codex => CODEX_ATTRIBUTES,
    };
    let mut attributes = BTreeMap::new();
    let mut duplicate_attribute = false;
    for attribute in record.attributes {
        if !allowlist.contains(&attribute.key.as_str()) {
            continue;
        }
        if attributes.insert(attribute.key, attribute.value).is_some() {
            duplicate_attribute = true;
        }
    }

    let Some(event_name) = attributes.get("event.name") else {
        return Ok(None);
    };
    let event_name = scalar_string(event_name).ok_or(OtelParseError::InvalidEvent)?;
    let is_target = match provider {
        OTelProvider::Claude => event_name == "api_request",
        OTelProvider::Codex => event_name == "codex.api_request",
    };
    if !is_target {
        return Ok(None);
    }
    if duplicate_attribute {
        return Err(OtelParseError::InvalidEvent);
    }

    let session_key = match provider {
        OTelProvider::Claude => "session.id",
        OTelProvider::Codex => "conversation.id",
    };
    let external_session_id = required_nonempty_string(&attributes, session_key)?;
    let duration_ms = required_u64(&attributes, "duration_ms")?;
    let duration = i64::try_from(duration_ms).map_err(|_| OtelParseError::InvalidEvent)?;
    let ended_at = match record.time_unix_nano {
        Some(value) => timestamp_from_unix_nanos(&value)?,
        None => {
            let value = required_nonempty_string(&attributes, "event.timestamp")?;
            value.parse().map_err(|_| OtelParseError::InvalidEvent)?
        }
    };
    let started_at = ended_at
        .checked_sub(SignedDuration::from_millis(duration))
        .map_err(|_| OtelParseError::InvalidEvent)?;
    let model = optional_nonempty_string(&attributes, "model")?;

    let (input_tokens, output_tokens, estimated_cost_usd_micros) = match provider {
        OTelProvider::Claude => (
            optional_u64(&attributes, "input_tokens")?,
            optional_u64(&attributes, "output_tokens")?,
            estimated_cost(&attributes)?,
        ),
        OTelProvider::Codex => (None, None, None),
    };

    Ok(Some(AuxiliaryFact {
        provider,
        external_session_id,
        started_at,
        ended_at,
        model,
        input_tokens,
        output_tokens,
        estimated_cost_usd_micros,
        duration_ms,
    }))
}

const CLAUDE_ATTRIBUTES: &[&str] = &[
    "event.name",
    "event.timestamp",
    "session.id",
    "model",
    "duration_ms",
    "input_tokens",
    "output_tokens",
    "cost_usd",
    "cost_usd_micros",
];

const CODEX_ATTRIBUTES: &[&str] = &[
    "event.name",
    "event.timestamp",
    "conversation.id",
    "model",
    "duration_ms",
];

fn required_nonempty_string(
    attributes: &BTreeMap<String, Value>,
    key: &str,
) -> Result<String, OtelParseError> {
    optional_nonempty_string(attributes, key)?.ok_or(OtelParseError::InvalidEvent)
}

fn optional_nonempty_string(
    attributes: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<String>, OtelParseError> {
    let Some(value) = attributes.get(key) else {
        return Ok(None);
    };
    let value = scalar_string(value).ok_or(OtelParseError::InvalidEvent)?;
    if value.is_empty() {
        return Err(OtelParseError::InvalidEvent);
    }
    Ok(Some(value.to_owned()))
}

fn required_u64(attributes: &BTreeMap<String, Value>, key: &str) -> Result<u64, OtelParseError> {
    optional_u64(attributes, key)?.ok_or(OtelParseError::InvalidEvent)
}

fn optional_u64(
    attributes: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<u64>, OtelParseError> {
    let Some(value) = attributes.get(key) else {
        return Ok(None);
    };
    scalar_u64(value)
        .map(Some)
        .ok_or(OtelParseError::InvalidEvent)
}

fn timestamp_from_unix_nanos(value: &Value) -> Result<Timestamp, OtelParseError> {
    let nanos = wire_i128(value)
        .filter(|value| *value >= 0)
        .ok_or(OtelParseError::InvalidEvent)?;
    Timestamp::from_nanosecond(nanos).map_err(|_| OtelParseError::InvalidEvent)
}

fn estimated_cost(attributes: &BTreeMap<String, Value>) -> Result<Option<u64>, OtelParseError> {
    let micros = optional_u64(attributes, "cost_usd_micros")?;
    let dollars = match attributes.get("cost_usd") {
        None => None,
        Some(value) => {
            let value = scalar_f64(value).ok_or(OtelParseError::InvalidEvent)?;
            if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 / 1_000_000.0 {
                return Err(OtelParseError::InvalidEvent);
            }
            Some((value * 1_000_000.0).round() as u64)
        }
    };
    match (micros, dollars) {
        (Some(left), Some(right)) if left != right => Err(OtelParseError::InvalidEvent),
        (Some(value), _) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn scalar_string(value: &Value) -> Option<&str> {
    value.get("stringValue")?.as_str()
}

fn wire_i128(value: &Value) -> Option<i128> {
    if let Some(value) = value.as_str() {
        return value.parse().ok();
    }
    if let Some(value) = value.as_u64() {
        return Some(i128::from(value));
    }
    if let Some(value) = value.as_i64() {
        return Some(i128::from(value));
    }
    scalar_i128(value)
}

fn scalar_i128(value: &Value) -> Option<i128> {
    if let Some(value) = value.get("stringValue").and_then(Value::as_str) {
        return value.parse().ok();
    }
    let value = value.get("intValue")?;
    if let Some(value) = value.as_str() {
        value.parse().ok()
    } else if let Some(value) = value.as_u64() {
        Some(i128::from(value))
    } else {
        value.as_i64().map(i128::from)
    }
}

fn scalar_u64(value: &Value) -> Option<u64> {
    scalar_i128(value)?.try_into().ok()
}

fn scalar_f64(value: &Value) -> Option<f64> {
    if let Some(value) = value.get("doubleValue") {
        return value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()));
    }
    scalar_i128(value).map(|value| value as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const END_NANOS: &str = "1785801610000000000";
    const SECRET: &str = "SUPER_SECRET_SENTINEL_2817";

    fn attr(key: &str, value: Value) -> Value {
        json!({"key": key, "value": value})
    }

    fn request(record: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "resourceLogs": [{
                "resource": {"attributes": [attr("authorization", json!({"stringValue": SECRET}))]},
                "scopeLogs": [{"scope": {"name": "fixture"}, "logRecords": [record]}]
            }]
        }))
        .unwrap()
    }

    fn claude_record() -> Value {
        json!({
            "timeUnixNano": END_NANOS,
            "body": {"stringValue": SECRET},
            "attributes": [
                attr("event.name", json!({"stringValue": "api_request"})),
                attr("event.timestamp", json!({"stringValue": "1999-01-01T00:00:00Z"})),
                attr("session.id", json!({"stringValue": "claude-session-1"})),
                attr("model", json!({"stringValue": "claude-sonnet-5"})),
                attr("duration_ms", json!({"intValue": "2000"})),
                attr("input_tokens", json!({"intValue": "120"})),
                attr("output_tokens", json!({"intValue": "30"})),
                attr("cost_usd_micros", json!({"intValue": "3210"})),
                attr("prompt", json!({"stringValue": SECRET})),
                attr("error", json!({"stringValue": SECRET})),
                attr("tool_input", json!({"stringValue": SECRET}))
            ]
        })
    }

    #[test]
    fn parses_claude_api_request_and_prefers_event_time() {
        let facts = parse_otlp_logs(OTelProvider::Claude, &request(claude_record())).unwrap();
        assert_eq!(facts.len(), 1);
        let fact = &facts[0];
        assert_eq!(fact.external_session_id, "claude-session-1");
        assert_eq!(fact.ended_at.to_string(), "2026-08-04T00:00:10Z");
        assert_eq!(fact.started_at.to_string(), "2026-08-04T00:00:08Z");
        assert_eq!(fact.input_tokens, Some(120));
        assert_eq!(fact.output_tokens, Some(30));
        assert_eq!(fact.estimated_cost_usd_micros, Some(3210));
        assert!(!format!("{fact:?}").contains(SECRET));
    }

    #[test]
    fn parses_codex_api_request_without_token_or_cost_fields() {
        let record = json!({
            "timeUnixNano": "1785801612000000000",
            "attributes": [
                attr("event.name", json!({"stringValue": "codex.api_request"})),
                attr("conversation.id", json!({"stringValue": "0198aabb-ccdd-7000-8000-000000000001"})),
                attr("model", json!({"stringValue": "gpt-5.6-sol"})),
                attr("duration_ms", json!({"stringValue": "1500"})),
                attr("input_token_count", json!({"intValue": "999"})),
                attr("output", json!({"stringValue": SECRET}))
            ]
        });
        let facts = parse_otlp_logs(OTelProvider::Codex, &request(record)).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].started_at.to_string(), "2026-08-04T00:00:10.5Z");
        assert_eq!(facts[0].ended_at.to_string(), "2026-08-04T00:00:12Z");
        assert_eq!(facts[0].input_tokens, None);
        assert_eq!(facts[0].output_tokens, None);
        assert_eq!(facts[0].estimated_cost_usd_micros, None);
        assert!(!format!("{:?}", facts[0]).contains(SECRET));
    }

    #[test]
    fn uses_source_time_for_late_arrival_and_iso_fallback() {
        let mut record = claude_record();
        record.as_object_mut().unwrap().remove("timeUnixNano");
        let attributes = record["attributes"].as_array_mut().unwrap();
        let timestamp = attributes
            .iter_mut()
            .find(|attribute| attribute["key"] == "event.timestamp")
            .unwrap();
        timestamp["value"]["stringValue"] = json!("2026-01-02T03:04:05Z");
        let fact = parse_otlp_logs(OTelProvider::Claude, &request(record))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(fact.ended_at.to_string(), "2026-01-02T03:04:05Z");
        assert_eq!(fact.started_at.to_string(), "2026-01-02T03:04:03Z");
    }

    #[test]
    fn ignores_unknown_events_even_when_their_payload_is_not_supported() {
        let record = json!({
            "attributes": [
                attr("event.name", json!({"stringValue": "tool_result"})),
                attr("duration_ms", json!({"intValue": "not-a-number"})),
                attr("duration_ms", json!({"arrayValue": {}})),
                attr("prompt", json!({"arrayValue": {"values": [SECRET]}}))
            ]
        });
        assert!(
            parse_otlp_logs(OTelProvider::Claude, &request(record))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_negative_overflow_and_nonfinite_allowlisted_values() {
        for (key, value) in [
            ("duration_ms", json!({"intValue": "-1"})),
            ("duration_ms", json!({"intValue": "18446744073709551616"})),
            ("cost_usd", json!({"doubleValue": "NaN"})),
            ("cost_usd", json!({"doubleValue": "inf"})),
        ] {
            let mut record = claude_record();
            let attributes = record["attributes"].as_array_mut().unwrap();
            attributes.retain(|attribute| attribute["key"] != key);
            attributes.push(attr(key, value));
            assert_eq!(
                parse_otlp_logs(OTelProvider::Claude, &request(record)),
                Err(OtelParseError::InvalidEvent)
            );
        }

        let mut record = claude_record();
        record["timeUnixNano"] = json!("999999999999999999999999999999999999999");
        assert_eq!(
            parse_otlp_logs(OTelProvider::Claude, &request(record)),
            Err(OtelParseError::InvalidEvent)
        );
    }

    #[test]
    fn rejects_invalid_target_without_echoing_secret() {
        let mut record = claude_record();
        let attributes = record["attributes"].as_array_mut().unwrap();
        attributes.retain(|attribute| attribute["key"] != "duration_ms");
        let error = parse_otlp_logs(OTelProvider::Claude, &request(record)).unwrap_err();
        assert_eq!(error, OtelParseError::InvalidEvent);
        assert!(!error.to_string().contains(SECRET));
    }

    #[test]
    fn enforces_one_mebibyte_input_limit() {
        let input = vec![b' '; MAX_INPUT_BYTES + 1];
        assert_eq!(
            parse_otlp_logs(OTelProvider::Claude, &input),
            Err(OtelParseError::InputTooLarge)
        );
    }
}
