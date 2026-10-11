//! OpenAI Decisions API (`POST /v1/decisions`, model `gpt-6-luna`).

use crate::http::Http;
use crate::ir::{Answer, Caps, QKind, Request, Response};
use crate::{Judge, JudgeError};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/decisions";

pub struct OpenAiDecisions {
    http: Http,
    model: String,
    name: String,
    caps: Caps,
}

impl OpenAiDecisions {
    pub fn new(
        name: &str,
        model: &str,
        endpoint: Option<&str>,
        api_key: &str,
        timeout: Duration,
        concurrency: usize,
        caps: Caps,
    ) -> Result<Self, JudgeError> {
        let url = endpoint.unwrap_or(DEFAULT_ENDPOINT).to_string();
        crate::http::validate_url(&url)?;
        let http = Http::new(url, vec![("Authorization".into(), format!("Bearer {api_key}"))], timeout, concurrency);
        Ok(OpenAiDecisions { http, model: model.into(), name: name.into(), caps })
    }
}

pub fn body(model: &str, req: &Request) -> Value {
    let input = if req.images.is_empty() {
        Value::String(req.text.clone())
    } else {
        let mut content = vec![json!({"type": "input_text", "text": req.text})];
        for img in &req.images {
            if let Some(c) = &img.caption {
                content.push(json!({"type": "input_text", "text": c}));
            }
            content.push(json!({"type": "input_image", "image_url": img.data_url()}));
        }
        json!([{"role": "user", "content": content}])
    };
    let questions: Vec<Value> = req
        .questions
        .iter()
        .map(|q| match &q.kind {
            QKind::Predicate => json!({"type": "predicate", "name": q.name, "instructions": q.instructions}),
            QKind::Choice { options } => json!({
                "type": "choice", "name": q.name, "instructions": q.instructions,
                "choices": options.iter().map(|(v, d)| json!({"value": v, "description": d})).collect::<Vec<_>>()
            }),
            QKind::Score { levels } => json!({
                "type": "score", "name": q.name, "instructions": q.instructions,
                "levels": levels.iter().map(|(l, d)| json!({"label": l, "description": d})).collect::<Vec<_>>()
            }),
        })
        .collect();
    json!({"model": model, "input": input, "questions": questions})
}

pub fn parse(v: &Value, req: &Request) -> Result<Response, JudgeError> {
    let arr = v.get("answers").and_then(Value::as_array).ok_or_else(|| JudgeError::Protocol("response has no `answers` array".into()))?;
    let mut out = Response::default();
    for (i, a) in arr.iter().enumerate() {
        let name = a
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| req.questions.get(i).map(|q| q.name.clone()))
            .ok_or_else(|| JudgeError::Protocol("answer without name".into()))?;
        let ty = a.get("type").and_then(Value::as_str).unwrap_or("");
        let ans = match ty {
            "predicate" => Answer::Predicate {
                p: a.get("probability").and_then(Value::as_f64).ok_or_else(|| JudgeError::Protocol(format!("{name}: no probability")))?,
            },
            "choice" => {
                let choice = match a.get("choice") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => return Err(JudgeError::Protocol(format!("{name}: no choice"))),
                };
                let probs = a
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .map(|ps| {
                        ps.iter()
                            .filter_map(|p| {
                                let v = match p.get("value")? {
                                    Value::String(s) => s.clone(),
                                    o => o.to_string(),
                                };
                                Some((v, p.get("probability")?.as_f64()?))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Answer::Choice { choice, probs, confidence: a.get("confidence").and_then(Value::as_f64) }
            }
            "score" => {
                let probs = a
                    .get("probabilities")
                    .and_then(Value::as_array)
                    .map(|ps| ps.iter().filter_map(|p| p.get("probability").and_then(Value::as_f64)).collect())
                    .unwrap_or_default();
                Answer::Score {
                    score: a.get("score").and_then(Value::as_f64).unwrap_or(0.0),
                    probs,
                    confidence: a.get("confidence").and_then(Value::as_f64),
                }
            }
            "refusal" => Answer::Refusal,
            other => return Err(JudgeError::Protocol(format!("{name}: unknown answer type `{other}`"))),
        };
        out.answers.insert(name, ans);
    }
    out.input_tokens = v.pointer("/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0);
    Ok(out)
}

impl Judge for OpenAiDecisions {
    fn id(&self) -> String {
        format!("{}:openai-decisions:{}", self.name, self.model)
    }
    fn caps(&self) -> Caps {
        self.caps
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let t = Instant::now();
        let v = self.http.post(&body(&self.model, req))?;
        let mut r = parse(&v, req)?;
        self.http.usage.input_tokens.fetch_add(r.input_tokens, std::sync::atomic::Ordering::Relaxed);
        r.latency_ms = t.elapsed().as_millis() as u64;
        r.judge = self.id();
        Ok(r)
    }
    fn usage(&self) -> Option<&crate::http::Usage> {
        Some(&self.http.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Question;
    #[test]
    fn body_and_parse_match_docs() {
        let req = Request {
            text: "I was charged twice".into(),
            images: vec![],
            questions: vec![
                Question::predicate("dup", "Is this a duplicate charge?"),
                Question::choice(
                    "dept",
                    "Which department?",
                    vec![("billing".into(), "Payments".into()), ("other".into(), "Other".into())],
                ),
                Question::score("sev", "How severe?", vec![("Cosmetic".into(), "".into()), ("Blocked".into(), "".into())]),
            ],
        };
        let b = body("gpt-6-luna", &req);
        assert_eq!(b["questions"][1]["choices"][0]["value"], "billing");
        assert_eq!(b["questions"][2]["levels"][1]["label"], "Blocked");
        assert_eq!(b["input"], "I was charged twice");
        let resp = json!({"answers": [
            {"type": "predicate", "name": "dup", "probability": 0.92},
            {"type": "choice", "name": "dept", "choice": "billing", "probabilities": [{"value": "billing", "probability": 0.95}, {"value": "other", "probability": 0.05}], "confidence": 0.93},
            {"type": "score", "name": "sev", "score": 1.1, "probabilities": [{"value": 0, "label": "Cosmetic", "probability": 0.1}, {"value": 1, "label": "Blocked", "probability": 0.9}], "confidence": 0.55},
        ]});
        let r = parse(&resp, &req).unwrap();
        assert_eq!(r.answers["dup"].p(), Some(0.92));
        assert!(matches!(&r.answers["dept"], Answer::Choice { choice, .. } if choice == "billing"));
        assert!(matches!(&r.answers["sev"], Answer::Score { score, probs, .. } if (*score - 1.1).abs() < 1e-9 && probs.len() == 2));
    }
}
