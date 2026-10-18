//! What came over the wire, kept before anything reads it.
//!
//! Governance Protocol § 4.5: "Raw tool output is archived before parsing,
//! so the record can distinguish decoder corruption from a parsing bug."
//! `misanthropic::Client::message` parses the response body itself and
//! drops the bytes, so a seat's response could only ever be stored after
//! serde had been through it. [`RawTransport`] is the send path that hands
//! the body back unparsed; [`Councillor`](crate::councillor::Councillor)
//! archives it (`prompt_archive_responses`, migration 085) and only then
//! parses it.
//!
//! [`EmissionBreach`] is the other half of § 4.5: a decision emitted out of
//! its declared order aborts the sitting. It carries what the record needs
//! to say why (the seat, the method, the declared order and the order the
//! keys came back in) and the evidence the abort seals (the request as
//! sent and the body as received).

use std::fmt;

use agora_common::enums::ModelRole;
use agora_common::ids::PromptArchiveId;
use agora_common::schema_guard::UnsoundEmission;

// The raw transport and body moved to agora-common (2026-10-17) so the
// Clerk archives its responses byte for byte as the seats do: one copy.
pub use agora_common::wire::{RawBody, RawTransport};

/// A seat's decision came back out of its declared order: the strict
/// emission contract (reason before commitment) did not hold, so the
/// sitting stops rather than record it (Governance Protocol § 4.5).
///
/// `Display` names the seat, the method and both key orders, nothing else;
/// `Debug` leaves out the request and the body, which go to the sealed
/// dump.
pub struct EmissionBreach {
    /// The seat whose call broke the contract.
    pub role: ModelRole,
    /// The model the response says it came from.
    pub model: String,
    pub response_id: String,
    /// The `record` method called.
    pub method: String,
    /// The method's properties, in the order its schema declares them.
    pub declared: Vec<String>,
    /// The keys of the tool input, in the order they came back.
    pub emitted: Vec<String>,
    /// What the check found.
    pub violation: UnsoundEmission,
    /// The archived prompt the response answers, when archiving is on;
    /// its raw body is in `prompt_archive_responses` under this id.
    pub prompt_archive_id: Option<PromptArchiveId>,
    /// The request exactly as sent.
    pub request: serde_json::Value,
    /// The response body exactly as received.
    pub body: RawBody,
}

impl EmissionBreach {
    /// The breach anywhere in `error`'s chain.
    pub fn find(error: &anyhow::Error) -> Option<&EmissionBreach> {
        error
            .chain()
            .find_map(|e| e.downcast_ref::<EmissionBreach>())
    }
}

impl fmt::Display for EmissionBreach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the {:?}'s `{}` call came back out of declared order (declared {:?}, \
             emitted {:?}): {}",
            self.role, self.method, self.declared, self.emitted, self.violation
        )
    }
}

impl fmt::Debug for EmissionBreach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmissionBreach")
            .field("role", &self.role)
            .field("model", &self.model)
            .field("response_id", &self.response_id)
            .field("method", &self.method)
            .field("declared", &self.declared)
            .field("emitted", &self.emitted)
            .field("violation", &self.violation)
            .field("prompt_archive_id", &self.prompt_archive_id)
            .field("body", &self.body)
            .finish_non_exhaustive()
    }
}

impl std::error::Error for EmissionBreach {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.violation)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agora_common::schema_guard::emission_follows_declaration;

    /// A `vote` submission whose vote arrives before its statement, as
    /// literal bytes, with spacing no serializer would produce.
    pub(crate) const REVERSED_VOTE: &str = r#"{"id":"msg_rev","type":"message","role":"assistant",
        "model":"claude-opus-4-6","content":[{"type":"text",
        "text":"{\"submission\" : \"vote\",\"vote\" : \"no\",\"statement\" : \"I support this; yes.\"}"}],
        "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

    /// The order the check sees is the order of the bytes: parsing a raw
    /// body keeps the submission's keys as emitted (`preserve_order`), so
    /// reversed keys reach `emission_follows_declaration` reversed and are
    /// caught. If parsing ever sorted them, `statement` would come before
    /// `vote` and the breach would pass silently.
    #[test]
    fn key_order_survives_from_raw_bytes_into_the_checked_map() {
        let body = RawBody::new(REVERSED_VOTE);
        let message = body.parse().expect("the body parses");
        let submitted: serde_json::Value = message.json().expect("a submission");
        let keys: Vec<&String> = submitted.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["submission", "vote", "statement"]);

        let declared = crate::record::declared_orders()
            .iter()
            .find(|(name, _)| name == "vote")
            .map(|(_, declared)| declared)
            .unwrap();
        assert_eq!(declared, &["submission", "statement", "vote"]);
        match emission_follows_declaration("vote", declared, &submitted) {
            Err(UnsoundEmission::OutOfOrder {
                field, preceding, ..
            }) => {
                assert_eq!(field, "statement");
                assert_eq!(preceding, "vote");
            }
            other => panic!("expected OutOfOrder, got {other:?}"),
        }

        // The bytes are kept as they came: not re-serialized.
        assert_eq!(body.as_bytes(), REVERSED_VOTE.as_bytes());
        let debug = format!("{body:?}");
        assert!(!debug.contains("I support this"), "{debug}");
        assert!(debug.contains(&body.sha256()), "{debug}");
    }
}
