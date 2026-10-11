//! Deterministic offline judge for tests and demos: predicates score the
//! word overlap between the question and the evidence text. Never use it to
//! make real decisions.

use crate::ir::{Answer, Caps, QKind, Request, Response};
use crate::{Judge, JudgeError};
use std::collections::HashSet;

pub struct Mock {
    name: String,
}

impl Mock {
    pub fn new(name: &str) -> Self {
        Mock { name: name.into() }
    }
}

fn words(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !matches!(*w, "the" | "and" | "does" | "item" | "that" | "this" | "with" | "for" | "from" | "are" | "can" | "any" | "match" | "picture" | "file" | "folder"))
        .map(|w| w.trim_end_matches('s').to_string())
        .collect()
}

impl Judge for Mock {
    fn id(&self) -> String {
        format!("{}:mock", self.name)
    }
    fn caps(&self) -> Caps {
        Caps { images: true, max_images: 8, max_questions: 64 }
    }
    fn decide(&self, req: &Request) -> Result<Response, JudgeError> {
        let text = words(&req.text);
        let mut r = Response { judge: self.id(), ..Default::default() };
        for q in &req.questions {
            let crit = q.instructions.split_once("CRITERION:").map(|x| x.1).unwrap_or(&q.instructions);
            let qw = words(crit);
            let overlap = if qw.is_empty() { 0.0 } else { qw.intersection(&text).count() as f64 / qw.len() as f64 };
            let a = match &q.kind {
                QKind::Predicate => Answer::Predicate { p: (0.02 + overlap * 0.96).min(0.98) },
                QKind::Choice { options } => {
                    let pick = options.iter().find(|(v, _)| text.contains(&v.to_lowercase())).or_else(|| options.last()).map(|o| o.0.clone()).unwrap_or_default();
                    let probs = options.iter().map(|(v, _)| (v.clone(), if *v == pick { 0.9 } else { 0.1 / options.len().max(1) as f64 })).collect();
                    Answer::Choice { choice: pick, probs, confidence: Some(0.8) }
                }
                QKind::Score { levels } => {
                    let mut probs = vec![0.0; levels.len()];
                    if let Some(f) = probs.first_mut() {
                        *f = 1.0;
                    }
                    Answer::Score { score: 0.0, probs, confidence: Some(0.9) }
                }
            };
            r.answers.insert(q.name.clone(), a);
        }
        Ok(r)
    }
}
