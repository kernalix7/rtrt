use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

pub const CURRENT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const MAX_IDENTIFIER_LEN: usize = 64;

/// Protocol version marker. Deserialization rejects every unsupported version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion;

impl ProtocolVersion {
    pub const CURRENT: Self = Self;

    pub const fn get(self) -> u16 {
        CURRENT_PROTOCOL_VERSION
    }
}

impl Serialize for ProtocolVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u16(CURRENT_PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = u16::deserialize(deserializer)?;
        if version == CURRENT_PROTOCOL_VERSION {
            Ok(Self)
        } else {
            Err(de::Error::custom(format_args!(
                "unsupported protocol version {version}; expected {CURRENT_PROTOCOL_VERSION}"
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdentifierError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
    #[error("{kind} exceeds {MAX_IDENTIFIER_LEN} bytes")]
    TooLong { kind: &'static str },
    #[error("{kind} must start with an ASCII letter or digit")]
    InvalidStart { kind: &'static str },
    #[error("{kind} contains a character outside [A-Za-z0-9_-]")]
    InvalidCharacter { kind: &'static str },
}

fn validate_identifier(value: &str, kind: &'static str) -> Result<(), IdentifierError> {
    if value.is_empty() {
        return Err(IdentifierError::Empty { kind });
    }
    if value.len() > MAX_IDENTIFIER_LEN {
        return Err(IdentifierError::TooLong { kind });
    }

    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(IdentifierError::InvalidStart { kind });
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
        return Err(IdentifierError::InvalidCharacter { kind });
    }
    Ok(())
}

macro_rules! identifier {
    ($name:ident, $kind:literal, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                validate_identifier(&value, $kind)?;
                Ok(Self(value))
            }

            pub fn generate() -> Self {
                Self(format!("{}_{}", $prefix, Uuid::new_v4().simple()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdentifierError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

identifier!(RunId, "run ID", "run");
identifier!(TaskId, "task ID", "task");
identifier!(MessageId, "message ID", "msg");
identifier!(IdempotencyKey, "idempotency key", "idem");

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("pinned SHA must be exactly 40 or 64 lowercase hexadecimal characters")]
pub struct PinnedShaError;

/// Canonical full object ID. Abbreviations and revision expressions are invalid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PinnedSha(String);

impl PinnedSha {
    pub fn new(value: impl Into<String>) -> Result<Self, PinnedShaError> {
        let value = value.into();
        if matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(PinnedShaError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PinnedSha {
    type Error = PinnedShaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for PinnedSha {
    type Error = PinnedShaError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl AsRef<str> for PinnedSha {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for PinnedSha {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for PinnedSha {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PinnedSha {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(u64);

impl Revision {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: ProtocolVersion,
    pub message_id: MessageId,
    pub idempotency_key: IdempotencyKey,
    pub revision: Revision,
    pub message: Message,
}

impl Envelope {
    pub fn new(
        message_id: MessageId,
        idempotency_key: IdempotencyKey,
        revision: Revision,
        message: Message,
    ) -> Self {
        Self {
            version: ProtocolVersion::CURRENT,
            message_id,
            idempotency_key,
            revision,
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Start(Start),
    Dispatch(Dispatch),
    Started(Started),
    Result(TaskResult),
    Cancel(Cancel),
    Reconcile(Reconcile),
    Complete(Complete),
    Failed(Failed),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum MessageKind {
    Hello,
    Start,
    Dispatch,
    Started,
    Result,
    Cancel,
    Reconcile,
    Complete,
    Failed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageWire {
    #[serde(rename = "type")]
    kind: MessageKind,
    body: serde_json::Value,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MessageWire::deserialize(deserializer)?;
        match wire.kind {
            MessageKind::Hello => parse_message_body(wire.body).map(Self::Hello),
            MessageKind::Start => parse_message_body(wire.body).map(Self::Start),
            MessageKind::Dispatch => parse_message_body(wire.body).map(Self::Dispatch),
            MessageKind::Started => parse_message_body(wire.body).map(Self::Started),
            MessageKind::Result => parse_message_body(wire.body).map(Self::Result),
            MessageKind::Cancel => parse_message_body(wire.body).map(Self::Cancel),
            MessageKind::Reconcile => parse_message_body(wire.body).map(Self::Reconcile),
            MessageKind::Complete => parse_message_body(wire.body).map(Self::Complete),
            MessageKind::Failed => parse_message_body(wire.body).map(Self::Failed),
        }
    }
}

fn parse_message_body<T, E>(body: serde_json::Value) -> Result<T, E>
where
    T: de::DeserializeOwned,
    E: de::Error,
{
    serde_json::from_value(body).map_err(E::custom)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    Host,
    Adapter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Starting,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Dispatched,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCompletionStatus {
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub role: PeerRole,
    pub implementation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub run_id: RunId,
    pub base_sha: PinnedSha,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dispatch {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub instruction: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Started {
    pub run_id: RunId,
    pub task_id: TaskId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskResult {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub outcome: TaskOutcome,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cancel {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reconcile {
    pub run_id: RunId,
    pub run_status: RunStatus,
    pub tasks: Vec<TaskSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSnapshot {
    pub task_id: TaskId,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    pub run_id: RunId,
    pub status: RunCompletionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failed {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("JSONL input must contain exactly one non-empty record")]
    InvalidJsonLine,
    #[error("JSONL record exceeds the maximum of {max_bytes} bytes")]
    JsonLineTooLong { max_bytes: usize },
    #[error("invalid protocol JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Serializes one envelope and appends exactly one JSONL record terminator.
pub fn encode_json_line(envelope: &Envelope) -> Result<String, ProtocolError> {
    let mut encoded = serde_json::to_string(envelope)?;
    if encoded.len() > MAX_JSON_LINE_BYTES {
        return Err(ProtocolError::JsonLineTooLong {
            max_bytes: MAX_JSON_LINE_BYTES,
        });
    }
    encoded.push('\n');
    Ok(encoded)
}

/// Parses exactly one JSONL record, with or without its final line terminator.
pub fn decode_json_line(line: &str) -> Result<Envelope, ProtocolError> {
    let record = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line);
    if record.len() > MAX_JSON_LINE_BYTES {
        return Err(ProtocolError::JsonLineTooLong {
            max_bytes: MAX_JSON_LINE_BYTES,
        });
    }
    if record.is_empty() || record.contains('\r') || record.contains('\n') {
        return Err(ProtocolError::InvalidJsonLine);
    }
    Ok(serde_json::from_str(record)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(message: Message) -> Envelope {
        Envelope::new(
            MessageId::new("msg_1").unwrap(),
            IdempotencyKey::new("idem_1").unwrap(),
            Revision::new(7),
            message,
        )
    }

    fn run_id() -> RunId {
        RunId::new("run_1").unwrap()
    }

    fn task_id() -> TaskId {
        TaskId::new("task_1").unwrap()
    }

    fn sha() -> PinnedSha {
        PinnedSha::new("0123456789abcdef0123456789abcdef01234567").unwrap()
    }

    fn messages() -> Vec<Message> {
        vec![
            Message::Hello(Hello {
                role: PeerRole::Adapter,
                implementation: "opencode-native".into(),
            }),
            Message::Start(Start {
                run_id: run_id(),
                base_sha: sha(),
            }),
            Message::Dispatch(Dispatch {
                run_id: run_id(),
                task_id: task_id(),
                instruction: "inspect only\nreturn findings".into(),
            }),
            Message::Started(Started {
                run_id: run_id(),
                task_id: task_id(),
            }),
            Message::Result(TaskResult {
                run_id: run_id(),
                task_id: task_id(),
                outcome: TaskOutcome::Succeeded,
                output: "result".into(),
            }),
            Message::Cancel(Cancel {
                run_id: run_id(),
                task_id: Some(task_id()),
                reason: "superseded".into(),
            }),
            Message::Reconcile(Reconcile {
                run_id: run_id(),
                run_status: RunStatus::Running,
                tasks: vec![TaskSnapshot {
                    task_id: task_id(),
                    status: TaskStatus::Running,
                }],
            }),
            Message::Complete(Complete {
                run_id: run_id(),
                status: RunCompletionStatus::Completed,
            }),
            Message::Failed(Failed {
                run_id: run_id(),
                task_id: None,
                code: "host_error".into(),
                message: "adapter disconnected".into(),
            }),
        ]
    }

    #[test]
    fn every_message_variant_round_trips_as_one_json_line() {
        for message in messages() {
            let expected = envelope(message);
            let line = encode_json_line(&expected).unwrap();
            assert!(line.ends_with('\n'));
            assert_eq!(line.bytes().filter(|byte| *byte == b'\n').count(), 1);
            assert_eq!(decode_json_line(&line).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_unknown_envelope_and_message_fields() {
        let valid = encode_json_line(&envelope(messages().remove(0))).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(decode_json_line(&value.to_string()).is_err());

        let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
        value["message"]["unexpected"] = serde_json::json!(true);
        assert!(decode_json_line(&value.to_string()).is_err());

        for message in messages() {
            let valid = encode_json_line(&envelope(message)).unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
            value["message"]["body"]["unexpected"] = serde_json::json!(true);
            assert!(decode_json_line(&value.to_string()).is_err());
        }
    }

    #[test]
    fn rejects_unsupported_versions_invalid_ids_and_revision_expressions() {
        let valid = encode_json_line(&envelope(Message::Hello(Hello {
            role: PeerRole::Host,
            implementation: "rtrt".into(),
        })))
        .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
        value["version"] = serde_json::json!(2);
        assert!(decode_json_line(&value.to_string()).is_err());

        assert!(RunId::new("../escape").is_err());
        assert!(TaskId::new("-option").is_err());
        assert!(MessageId::new("contains space").is_err());
        assert!(PinnedSha::new("HEAD").is_err());
        assert!(PinnedSha::new("0123456789ABCDEF0123456789ABCDEF01234567").is_err());
        assert!(PinnedSha::new("0123456789abcdef0123456789abcdef01234567^{}").is_err());
    }

    #[test]
    fn rejects_missing_duplicate_and_invalid_message_discriminators() {
        let valid = encode_json_line(&envelope(Message::Started(Started {
            run_id: run_id(),
            task_id: task_id(),
        })))
        .unwrap();

        for field in ["type", "body"] {
            let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
            value["message"].as_object_mut().unwrap().remove(field);
            assert!(decode_json_line(&value.to_string()).is_err());
        }

        let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
        value["message"]["type"] = serde_json::json!("future_message");
        assert!(decode_json_line(&value.to_string()).is_err());

        let duplicate_type = valid.replacen(
            "\"type\":\"started\"",
            "\"type\":\"started\",\"type\":\"started\"",
            1,
        );
        assert!(decode_json_line(&duplicate_type).is_err());
    }

    #[test]
    fn accepts_only_the_exact_integer_protocol_version() {
        let valid = encode_json_line(&envelope(Message::Hello(Hello {
            role: PeerRole::Host,
            implementation: "rtrt".into(),
        })))
        .unwrap();

        for version in [
            serde_json::json!(0),
            serde_json::json!(2),
            serde_json::json!(-1),
            serde_json::json!(1.0),
            serde_json::json!("1"),
            serde_json::Value::Null,
        ] {
            let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
            value["version"] = version;
            assert!(decode_json_line(&value.to_string()).is_err());
        }

        let duplicate_version = valid.replacen("\"version\":1", "\"version\":1,\"version\":1", 1);
        assert!(decode_json_line(&duplicate_version).is_err());
    }

    #[test]
    fn validates_identifier_and_sha_boundaries() {
        assert!(RunId::new("a".repeat(MAX_IDENTIFIER_LEN)).is_ok());
        assert!(matches!(
            RunId::new("a".repeat(MAX_IDENTIFIER_LEN + 1)),
            Err(IdentifierError::TooLong { kind: "run ID" })
        ));

        for invalid in ["_prefix", "-prefix", "non.ascii", "한글", "has/slash"] {
            assert!(RunId::new(invalid).is_err());
        }

        assert!(PinnedSha::new("0".repeat(40)).is_ok());
        assert!(PinnedSha::new("f".repeat(64)).is_ok());
        for invalid in [
            "0".repeat(39),
            "0".repeat(41),
            "f".repeat(63),
            "f".repeat(65),
            format!("{}g", "0".repeat(39)),
            "A".repeat(40),
        ] {
            assert!(PinnedSha::new(invalid).is_err());
        }
    }

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        let first = RunId::generate();
        let second = RunId::generate();
        assert_ne!(first, second);
        assert!(RunId::new(first.as_str()).is_ok());
        assert!(TaskId::new(TaskId::generate().as_str()).is_ok());
    }

    #[test]
    fn rejects_empty_and_multiple_jsonl_records() {
        let line = encode_json_line(&envelope(Message::Hello(Hello {
            role: PeerRole::Host,
            implementation: "rtrt".into(),
        })))
        .unwrap();
        assert!(matches!(
            decode_json_line(""),
            Err(ProtocolError::InvalidJsonLine)
        ));
        assert!(matches!(
            decode_json_line(&(line.clone() + &line)),
            Err(ProtocolError::InvalidJsonLine)
        ));
        assert!(decode_json_line(line.trim_end()).is_ok());
        assert!(decode_json_line(&format!("{}\r\n", line.trim_end())).is_ok());
        assert!(matches!(
            decode_json_line(&format!("{}\r", line.trim_end())),
            Err(ProtocolError::InvalidJsonLine)
        ));
        assert!(matches!(
            decode_json_line(&(line.clone() + "\n")),
            Err(ProtocolError::InvalidJsonLine)
        ));
    }

    #[test]
    fn enforces_json_record_size_on_encode_and_decode() {
        let base = envelope(Message::Result(TaskResult {
            run_id: run_id(),
            task_id: task_id(),
            outcome: TaskOutcome::Succeeded,
            output: String::new(),
        }));
        let base_len = serde_json::to_string(&base).unwrap().len();
        let payload_len = MAX_JSON_LINE_BYTES - base_len;

        let at_limit = envelope(Message::Result(TaskResult {
            run_id: run_id(),
            task_id: task_id(),
            outcome: TaskOutcome::Succeeded,
            output: "x".repeat(payload_len),
        }));
        let line = encode_json_line(&at_limit).unwrap();
        assert_eq!(line.len(), MAX_JSON_LINE_BYTES + 1);
        assert_eq!(decode_json_line(&line).unwrap(), at_limit);

        let over_limit = envelope(Message::Result(TaskResult {
            run_id: run_id(),
            task_id: task_id(),
            outcome: TaskOutcome::Succeeded,
            output: "x".repeat(payload_len + 1),
        }));
        assert!(matches!(
            encode_json_line(&over_limit),
            Err(ProtocolError::JsonLineTooLong {
                max_bytes: MAX_JSON_LINE_BYTES
            })
        ));

        let oversized_record = format!("{} ", line.trim_end());
        assert!(matches!(
            decode_json_line(&oversized_record),
            Err(ProtocolError::JsonLineTooLong {
                max_bytes: MAX_JSON_LINE_BYTES
            })
        ));
    }
}
