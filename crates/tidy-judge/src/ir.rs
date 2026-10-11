//! Provider-neutral question/answer representation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum QKind {
    /// Probability that a condition holds.
    Predicate,
    /// One of a fixed set of values (value, description).
    Choice { options: Vec<(String, String)> },
    /// Ordered levels (label, description), lowest first.
    Score { levels: Vec<(String, String)> },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Question {
    pub name: String,
    pub instructions: String,
    pub kind: QKind,
}

impl Question {
    pub fn predicate(name: impl Into<String>, instructions: impl Into<String>) -> Self {
        Question { name: name.into(), instructions: instructions.into(), kind: QKind::Predicate }
    }
    pub fn choice(name: impl Into<String>, instructions: impl Into<String>, options: Vec<(String, String)>) -> Self {
        Question { name: name.into(), instructions: instructions.into(), kind: QKind::Choice { options } }
    }
    pub fn score(name: impl Into<String>, instructions: impl Into<String>, levels: Vec<(String, String)>) -> Self {
        Question { name: name.into(), instructions: instructions.into(), kind: QKind::Score { levels } }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Image {
    /// image/jpeg, image/png, image/webp
    pub mime: String,
    /// Encoded bytes.
    #[serde(with = "b64")]
    pub data: Vec<u8>,
    /// Optional caption placed before the image (e.g. "Reference photo of `me`").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

impl Image {
    pub fn data_url(&self) -> String {
        use base64::Engine;
        format!("data:{};base64,{}", self.mime, base64::engine::general_purpose::STANDARD.encode(&self.data))
    }
}

/// One decision request: shared evidence plus independent questions.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
    pub questions: Vec<Question>,
}

impl Request {
    /// Stable cache key component (independent of question order).
    pub fn digest(&self) -> String {
        let mut qs = self.questions.clone();
        qs.sort_by(|a, b| a.name.cmp(&b.name));
        let mut h = blake3::Hasher::new();
        h.update(self.text.as_bytes());
        for i in &self.images {
            h.update(i.mime.as_bytes());
            h.update(blake3::hash(&i.data).as_bytes());
            h.update(i.caption.as_deref().unwrap_or("").as_bytes());
        }
        h.update(serde_json::to_string(&qs).unwrap_or_default().as_bytes());
        h.finalize().to_hex().to_string()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Predicate {
        p: f64,
    },
    Choice {
        choice: String,
        probs: Vec<(String, f64)>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        probs: Vec<f64>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// The model declined to answer this question.
    Refusal,
}

impl Answer {
    pub fn p(&self) -> Option<f64> {
        match self {
            Answer::Predicate { p } => Some(*p),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub latency_ms: u64,
    /// Which judge produced this (after cascading).
    #[serde(default)]
    pub judge: String,
    #[serde(default)]
    pub cached: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub images: bool,
    pub max_images: usize,
    pub max_questions: usize,
}
