//! Plain-English requests ("trash installers in Downloads older than 30
//! days") turned into a one-off rule. The parts Tidy can check in code —
//! the action, the folder, age, size, kind and destination — become the
//! rule's filters; whatever is left describes the items, and the judge
//! decides which items match it. Nothing here touches a file.

use crate::policy::{Filter, Rule, To, When};
use std::path::{Path, PathBuf};
use tdy_plan::{Action, Mode};
use tdy_scan::path::{display_path, expand_tilde};

/// A request, understood.
#[derive(Clone, Debug)]
pub struct Request {
    /// What was typed.
    pub text: String,
    pub action: Action,
    /// Where to look (everything inside counts).
    pub scope: PathBuf,
    /// What the items are, for the judge; `None` when the filters say it all.
    pub criterion: Option<String>,
    pub filter: Filter,
    pub dest: Option<PathBuf>,
    pub tags: Vec<String>,
}

const ID: &str = "ask";

impl Request {
    /// The one-off rule. Its entries always wait for your review.
    pub fn rule(&self) -> Rule {
        Rule {
            id: ID.into(),
            action: self.action.as_str().into(),
            scope: vec![self.scope.to_string_lossy().into_owned()],
            filter: self.filter.clone(),
            when: When {
                all: self.criterion.iter().map(|c| format!("{} (the owner asked: \"{}\")", capitalise(c), self.text.trim())).collect(),
                ..Default::default()
            },
            to: self.dest.as_ref().map(|d| To {
                path: Some(d.to_string_lossy().into_owned()),
                create_if_missing: true,
                ..Default::default()
            }),
            tags: self.tags.clone(),
            on_conflict: None,
            threshold: None,
            mode: Some(Mode::Manual),
            judge: None,
            content: None,
            name: None,
            protects: None,
            check: None,
            always_review: false,
            preset: None,
        }
    }

    /// One line per part, for showing what was understood before it runs.
    pub fn describe(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![("do", self.action_text())];
        v.push(("in", format!("{} and everything inside", display_path(&self.scope))));
        let f = &self.filter;
        let mut which = vec![];
        if let Some(k) = &f.kind {
            which.push(kind_label(k).to_string());
        }
        if !f.ext.is_empty() {
            which.push(f.ext.iter().map(|e| format!(".{e}")).collect::<Vec<_>>().join(" "));
        }
        if which.is_empty() {
            which.push("files".into());
        }
        if let Some(a) = &f.older_than {
            which.push(format!("older than {}", age_text(a)));
        }
        if let Some(a) = &f.newer_than {
            which.push(format!("changed in the last {}", age_text(a)));
        }
        if let Some(s) = &f.min_size {
            which.push(format!("over {s}"));
        }
        if let Some(s) = &f.max_size {
            which.push(format!("under {s}"));
        }
        v.push(("which", which.join(" · ")));
        v.push(match &self.criterion {
            Some(c) => ("judge", format!("picks those that are “{c}”")),
            None => ("judge", "not needed: the filters say it all".into()),
        });
        v
    }

    fn action_text(&self) -> String {
        match self.action {
            Action::Move => format!("move to {}", self.dest.as_deref().map(display_path).unwrap_or_default()),
            Action::Copy => format!("copy to {}", self.dest.as_deref().map(display_path).unwrap_or_default()),
            Action::Tag => format!("tag as {}", self.tags.join(", ")),
            Action::Trash => "trash (say “permanently delete” to skip the trash)".into(),
            Action::Delete => "delete permanently (can't be undone)".into(),
            a => a.as_str().into(),
        }
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn kind_label(k: &str) -> &str {
    match k {
        "dir" => "folders",
        "image" => "images",
        "video" => "videos",
        "audio" => "audio files",
        "document" => "documents",
        "archive" => "archives",
        "installer" => "installers and disk images",
        "code" => "source files",
        other => other,
    }
}

/// "30d" → "30 days".
fn age_text(a: &str) -> String {
    let split = a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len());
    let (n, u) = a.split_at(split);
    let unit = match u {
        "d" => "day",
        "w" => "week",
        "mo" => "month",
        "y" => "year",
        other => other,
    };
    format!("{n} {unit}{}", if n == "1" { "" } else { "s" })
}

/// One word, or a quoted phrase.
#[derive(Clone, Debug)]
struct Tok {
    text: String,
    quoted: bool,
}

impl Tok {
    fn low(&self) -> String {
        self.text.to_lowercase()
    }
    fn is(&self, words: &[&str]) -> bool {
        !self.quoted && words.contains(&self.low().as_str())
    }
}

fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q || (q == '“' && c == '”') => {
                out.push(Tok { text: std::mem::take(&mut cur), quoted: true });
                quote = None;
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'' | '“') if cur.is_empty() => quote = Some(if ch == '“' { '“' } else { ch }),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(Tok { text: std::mem::take(&mut cur), quoted: false });
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(Tok { text: cur, quoted: quote.is_some() });
    }
    // trailing punctuation belongs to the sentence, not the word
    for t in &mut out {
        if !t.quoted {
            while t.text.len() > 1 && t.text.ends_with(['.', ',', ';', '!', '?']) {
                t.text.pop();
            }
        }
    }
    out
}

/// The action and how many words it took.
fn verb(t: &[Tok]) -> Option<(Action, usize)> {
    let w = |i: usize| t.get(i).map(Tok::low).unwrap_or_default();
    Some(match w(0).as_str() {
        "trash" | "bin" | "remove" | "delete" | "discard" => (Action::Trash, 1),
        "throw" if w(1) == "away" || w(1) == "out" => (Action::Trash, 2),
        "get" if w(1) == "rid" && w(2) == "of" => (Action::Trash, 3),
        "clean" if w(1) == "up" || w(1) == "out" => (Action::Trash, 2),
        "permanently" if w(1) == "delete" || w(1) == "remove" => (Action::Delete, 2),
        "quarantine" => (Action::Quarantine, 1),
        "set" if w(1) == "aside" => (Action::Quarantine, 2),
        "move" => (Action::Move, 1),
        "copy" => (Action::Copy, 1),
        "tag" | "label" => (Action::Tag, 1),
        "archive" => (Action::Archive, 1),
        "compress" => (Action::Compress, 1),
        _ => return None,
    })
}

fn number(t: &Tok) -> Option<f64> {
    let s = t.low();
    let words = ["a", "an", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"];
    if let Some(i) = words.iter().position(|w| *w == s) {
        return Some(if i < 2 { 1.0 } else { (i - 1) as f64 });
    }
    s.parse().ok()
}

fn duration_unit(s: &str) -> Option<&'static str> {
    Some(match s {
        "d" | "day" | "days" => "d",
        "w" | "wk" | "wks" | "week" | "weeks" => "w",
        "mo" | "month" | "months" => "mo",
        "y" | "yr" | "yrs" | "year" | "years" => "y",
        _ => return None,
    })
}

fn size_unit(s: &str) -> Option<&'static str> {
    Some(match s {
        "b" | "bytes" => "B",
        "k" | "kb" | "kib" => "KB",
        "m" | "mb" | "mib" | "meg" | "megs" => "MB",
        "g" | "gb" | "gib" | "gig" | "gigs" => "GB",
        "t" | "tb" | "tib" => "TB",
        _ => return None,
    })
}

enum Quantity {
    Age(String),
    Size(String),
}

/// "30 days", "30d", "a year", "500MB", "1.5 GB" at `t[i..]`, with how many
/// words it took.
fn quantity(t: &[Tok], i: usize) -> Option<(Quantity, usize)> {
    let first = t.get(i)?;
    // one word: 30d, 6mo, 500MB
    let s = first.low();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    if split > 0 && split < s.len() {
        let (n, u) = s.split_at(split);
        if let Some(u) = duration_unit(u) {
            return Some((Quantity::Age(format!("{}{u}", n.parse::<f64>().ok()?.round() as i64)), 1));
        }
        if let Some(u) = size_unit(u) {
            return Some((Quantity::Size(format!("{n}{u}")), 1));
        }
        return None;
    }
    // two words: 30 days, a year, 1.5 GB
    let n = number(first)?;
    let u = t.get(i + 1)?.low();
    if let Some(u) = duration_unit(&u) {
        return Some((Quantity::Age(format!("{}{u}", n.round() as i64)), 2));
    }
    size_unit(&u).map(|u| (Quantity::Size(format!("{n}{u}")), 2))
}

/// A folder named by `t`: a path, or a name found in `here` or in home.
fn folder(t: &Tok, here: &Path) -> Option<PathBuf> {
    let s = t.text.as_str();
    let pathlike = t.quoted || s.starts_with(['~', '/', '.']) || s.contains('/');
    let candidates = if s.starts_with(['~', '/']) { vec![expand_tilde(s)] } else { vec![here.join(s), tdy_scan::path::home_dir().join(s)] };
    let found = candidates.into_iter().find(|p| p.is_dir()).and_then(|p| std::fs::canonicalize(p).ok());
    // a bare word that isn't a folder is part of the description ("photos taken in Paris")
    if found.is_none() && !pathlike {
        return None;
    }
    found
}

/// A destination: like `folder`, but it may not exist yet.
fn destination(t: &Tok, here: &Path) -> Option<PathBuf> {
    if let Some(f) = folder(t, here) {
        return Some(f);
    }
    let s = t.text.as_str();
    if s.starts_with(['~', '/']) {
        return Some(expand_tilde(s));
    }
    if t.quoted || s.starts_with('.') || s.contains('/') {
        return Some(here.join(s));
    }
    None
}

const DETERMINERS: &[&str] = &["all", "the", "my", "any", "every", "each", "of", "those", "these", "that", "which", "are"];
const GENERIC: &[&str] = &["files", "file", "everything", "anything", "stuff", "items", "things", "entries"];

/// Nouns a filter can stand for. `exact`: the filter says it all, so the
/// judge isn't needed when the description is just this word.
fn noun_kind(w: &str) -> Option<(&'static str, bool)> {
    Some(match w {
        "installer" | "installers" => ("installer", true),
        "folder" | "folders" | "directories" | "directory" => ("dir", true),
        "image" | "images" => ("image", true),
        "video" | "videos" => ("video", true),
        "archive" | "archives" => ("archive", true),
        "document" | "documents" | "docs" => ("document", true),
        "photo" | "photos" | "picture" | "pictures" | "screenshot" | "screenshots" | "pics" => ("image", false),
        "movie" | "movies" | "recordings" | "clips" => ("video", false),
        "song" | "songs" | "music" | "podcasts" => ("audio", false),
        _ => return None,
    })
}

/// File extensions named directly ("dmg files", ".zip", "pdfs").
fn noun_ext(w: &str) -> Option<String> {
    let w = w.trim_start_matches('.');
    let w = w.strip_suffix("s").filter(|b| crate::items::kind_of_ext(b) != "file").unwrap_or(w);
    (crate::items::kind_of_ext(w) != "file").then(|| w.to_string())
}

/// Understand `text`; folders named without a path are looked for in
/// `here`, then in home. The error says what to write instead.
pub fn parse(text: &str, here: &Path) -> Result<Request, String> {
    let t = tokenize(text);
    let Some((action, used)) = verb(&t) else {
        return Err(
            "start with what to do: trash, permanently delete, quarantine, move … to …, copy … to …, tag … as …, archive or compress"
                .into(),
        );
    };
    let mut req = Request {
        text: text.trim().to_string(),
        action,
        scope: here.to_path_buf(),
        criterion: None,
        filter: Filter::default(),
        dest: None,
        tags: vec![],
    };
    let mut rest: Vec<&Tok> = vec![];
    let mut scoped = false;
    let mut i = used;
    while i < t.len() {
        let w = t[i].low();
        let next = |k: usize| t.get(i + k).map(Tok::low).unwrap_or_default();
        // move/copy … to <folder>
        if matches!(action, Action::Move | Action::Copy)
            && t[i].is(&["to", "into"])
            && let Some(d) = t.get(i + 1).and_then(|x| destination(x, here))
        {
            req.dest = Some(d);
            i += 2;
            continue;
        }
        // tag … as <name> / with <name>
        if action == Action::Tag && t[i].is(&["as", "with"]) {
            let skip = usize::from(next(1) == "tag" || next(1) == "label");
            if let Some(tag) = t.get(i + 1 + skip) {
                req.tags.push(tag.text.clone());
                i += 2 + skip;
                continue;
            }
        }
        // older: older than N, not used/opened/… in|for N, untouched for N, over N old
        let older_at = match w.as_str() {
            "older" if next(1) == "than" => Some(2),
            "untouched" | "unused" | "unopened" | "unchanged" if next(1) == "for" || next(1) == "in" => Some(2),
            "not" if ["used", "opened", "touched", "modified", "changed", "accessed", "edited", "looked"].contains(&next(1).as_str()) => {
                let mut k = 2;
                if next(k) == "at" {
                    k += 1;
                }
                if ["in", "for", "since"].contains(&next(k).as_str()) {
                    k += 1;
                }
                if next(k) == "the" && (next(k + 1) == "last" || next(k + 1) == "past") {
                    k += 2;
                }
                if next(k) == "over" || (next(k) == "more" && next(k + 1) == "than") {
                    k += if next(k) == "over" { 1 } else { 2 };
                }
                Some(k)
            }
            _ => None,
        };
        if let Some(k) = older_at
            && let Some((Quantity::Age(a), n)) = quantity(&t, i + k)
        {
            req.filter.older_than = Some(a);
            i += k + n;
            continue;
        }
        // newer: newer than N, in|from|within the last N, changed/added in the last N
        let newer_at = match w.as_str() {
            "newer" if next(1) == "than" => Some(2),
            "in" | "from" | "within" | "over" if next(1) == "the" && (next(2) == "last" || next(2) == "past") => Some(3),
            "modified" | "changed" | "added" | "downloaded" | "created" | "made"
                if (next(1) == "in" || next(1) == "within") && next(2) == "the" && (next(3) == "last" || next(3) == "past") =>
            {
                Some(4)
            }
            _ => None,
        };
        if let Some(k) = newer_at
            && let Some((Quantity::Age(a), n)) = quantity(&t, i + k)
        {
            req.filter.newer_than = Some(a);
            i += k + n;
            continue;
        }
        // size, or "over N <time> old"
        let bigger_at = match w.as_str() {
            "bigger" | "larger" | "greater" if next(1) == "than" => Some(2),
            "over" | "above" | "exceeding" => Some(1),
            "more" if next(1) == "than" => Some(2),
            "at" if next(1) == "least" => Some(2),
            _ => None,
        };
        if let Some(k) = bigger_at {
            match quantity(&t, i + k) {
                Some((Quantity::Size(s), n)) => {
                    req.filter.min_size = Some(s);
                    i += k + n;
                    continue;
                }
                // "over a year old", "more than 2 years old"
                Some((Quantity::Age(a), n)) => {
                    req.filter.older_than = Some(a);
                    i += k + n + usize::from(t.get(i + k + n).is_some_and(|x| x.is(&["old", "ago"])));
                    continue;
                }
                None => {}
            }
        }
        let smaller_at = match w.as_str() {
            "smaller" | "less" if next(1) == "than" => Some(2),
            "under" | "below" => Some(1),
            _ => None,
        };
        if let Some(k) = smaller_at
            && let Some((Quantity::Size(s), n)) = quantity(&t, i + k)
        {
            req.filter.max_size = Some(s);
            i += k + n;
            continue;
        }
        // N <time> old
        if let Some((Quantity::Age(a), n)) = quantity(&t, i)
            && t.get(i + n).is_some_and(|x| x.is(&["old"]))
        {
            req.filter.older_than = Some(a);
            i += n + 1;
            continue;
        }
        // in|from|under|inside|within [my|the] <folder> [folder]
        if !scoped && t[i].is(&["in", "from", "under", "inside", "within"]) {
            let mut k = 1;
            while t.get(i + k).is_some_and(|x| x.is(&["my", "the"])) {
                k += 1;
            }
            if let Some(f) = t.get(i + k).and_then(|x| folder(x, here)) {
                req.scope = f;
                scoped = true;
                i += k + 1 + usize::from(t.get(i + k + 1).is_some_and(|x| x.is(&["folder", "directory", "dir"])));
                continue;
            }
        }
        rest.push(&t[i]);
        i += 1;
    }
    // what's left describes the items
    while rest.first().is_some_and(|x| x.is(DETERMINERS)) {
        rest.remove(0);
    }
    while rest.last().is_some_and(|x| x.is(&["that", "which", "are", "is", "and"])) {
        rest.pop();
    }
    let words: Vec<String> = rest.iter().map(|x| x.low()).collect();
    let mut exact = true;
    let mut described = false;
    for w in &words {
        if GENERIC.contains(&w.as_str()) || DETERMINERS.contains(&w.as_str()) {
            continue;
        }
        if let Some((k, is_exact)) = noun_kind(w) {
            req.filter.kind.get_or_insert_with(|| k.to_string());
            exact &= is_exact;
            described = true;
            continue;
        }
        if let Some(e) = noun_ext(w) {
            if !req.filter.ext.contains(&e) {
                req.filter.ext.push(e);
            }
            described = true;
            continue;
        }
        // any other word needs the judge
        exact = false;
        described = true;
    }
    if !req.filter.ext.is_empty() {
        // named extensions are more precise than a kind
        req.filter.kind = None;
    }
    if described && !exact {
        req.criterion = Some(rest.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" "));
    }
    match action {
        Action::Move | Action::Copy if req.dest.is_none() => {
            return Err(format!("say where to {} them: “… to ~/Archive”", action.as_str()));
        }
        Action::Tag if req.tags.is_empty() => return Err("say which tag: “… as Receipts”".into()),
        _ => {}
    }
    if !described && req.filter.older_than.is_none() && req.filter.min_size.is_none() && req.filter.newer_than.is_none() {
        return Err("say which items: “installers”, “screenshots older than a month”, “videos over 1 GB”…".into());
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for d in ["Downloads", "Archive/old", "My Stuff"] {
            std::fs::create_dir_all(t.path().join(d)).unwrap();
        }
        t
    }

    #[test]
    fn installers_in_downloads() {
        let t = tree();
        let r = parse("trash installers in Downloads older than 30 days", t.path()).unwrap();
        assert_eq!(r.action, Action::Trash);
        assert_eq!(r.scope, std::fs::canonicalize(t.path().join("Downloads")).unwrap());
        assert_eq!(r.filter.kind.as_deref(), Some("installer"));
        assert_eq!(r.filter.older_than.as_deref(), Some("30d"));
        assert_eq!(r.criterion, None, "installers are known by their extension");
        let rule = r.rule();
        assert!(rule.when.is_empty());
        assert_eq!(rule.mode, Some(Mode::Manual));
    }

    #[test]
    fn descriptions_go_to_the_judge() {
        let t = tree();
        let r = parse("Quarantine screenshots not opened in the last 6 months", t.path()).unwrap();
        assert_eq!(r.action, Action::Quarantine);
        assert_eq!(r.scope, t.path());
        assert_eq!(r.filter.kind.as_deref(), Some("image"));
        assert_eq!(r.filter.older_than.as_deref(), Some("6mo"));
        assert_eq!(r.criterion.as_deref(), Some("screenshots"));
        let rule = r.rule();
        assert!(rule.when.all[0].starts_with("Screenshots (the owner asked:"));

        let r = parse("trash old tax receipts from 2019 in “My Stuff”", t.path()).unwrap();
        assert_eq!(r.criterion.as_deref(), Some("old tax receipts from 2019"));
        assert!(r.scope.ends_with("My Stuff"));
        // a word after "in" that isn't a folder stays in the description
        let r = parse("trash photos taken in Paris", t.path()).unwrap();
        assert_eq!(r.criterion.as_deref(), Some("photos taken in Paris"));
        assert_eq!(r.scope, t.path());
    }

    #[test]
    fn sizes_ages_and_destinations() {
        let t = tree();
        let r = parse("move videos over 1.5 GB to ~/Archive/old", t.path()).unwrap();
        assert_eq!(r.action, Action::Move);
        assert_eq!(r.filter.min_size.as_deref(), Some("1.5GB"));
        assert_eq!(r.filter.kind.as_deref(), Some("video"));
        assert!(r.dest.as_ref().unwrap().ends_with("Archive/old"));
        assert!(r.rule().to.unwrap().create_if_missing);

        let r = parse("copy pdfs changed in the last 2 weeks into \"Archive\"", t.path()).unwrap();
        assert_eq!(r.filter.ext, ["pdf"]);
        assert_eq!(r.filter.newer_than.as_deref(), Some("2w"));
        assert!(r.dest.unwrap().ends_with("Archive"));

        let r = parse("get rid of dmg files more than a year old", t.path()).unwrap();
        assert_eq!(r.action, Action::Trash);
        assert_eq!(r.filter.ext, ["dmg"]);
        assert_eq!(r.filter.older_than.as_deref(), Some("1y"));
        assert_eq!(r.criterion, None);

        let r = parse("permanently delete files under 1KB untouched for 2y.", t.path()).unwrap();
        assert_eq!(r.action, Action::Delete);
        assert_eq!(r.filter.max_size.as_deref(), Some("1KB"));
        assert_eq!(r.filter.older_than.as_deref(), Some("2y"));

        let r = parse("tag invoices as Receipts", t.path()).unwrap();
        assert_eq!(r.tags, ["Receipts"]);
        assert_eq!(r.criterion.as_deref(), Some("invoices"));
    }

    #[test]
    fn says_what_is_missing() {
        let t = tree();
        assert!(parse("tidy up my photos", t.path()).unwrap_err().starts_with("start with what to do"));
        assert!(parse("move screenshots", t.path()).unwrap_err().contains("where to move"));
        assert!(parse("tag invoices", t.path()).unwrap_err().contains("which tag"));
        assert!(parse("trash everything", t.path()).unwrap_err().contains("which items"));
        // but a filter alone is enough
        assert!(parse("trash everything older than 3 years", t.path()).is_ok());
    }

    #[test]
    fn describes_itself() {
        let t = tree();
        let r = parse("trash screenshots in Downloads older than 30 days", t.path()).unwrap();
        let d: Vec<String> = r.describe().into_iter().map(|(k, v)| format!("{k}: {v}")).collect();
        assert!(d[0].starts_with("do: trash"));
        assert!(d[2].contains("images · older than 30 days"), "{d:?}");
        assert!(d[3].contains("“screenshots”"));
    }
}
