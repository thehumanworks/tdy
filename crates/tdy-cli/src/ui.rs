//! Terminal rendering helpers.

use std::io::IsTerminal;
use tdy_scan::TreeRow;
use tdy_scan::fmt;

pub fn color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

pub fn dim(s: &str) -> String {
    if color() { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() }
}

pub fn bold(s: &str) -> String {
    if color() { format!("\x1b[1m{s}\x1b[0m") } else { s.to_string() }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn bar(frac: f64, width: usize) -> String {
    let filled = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut s = "█".repeat(filled);
    s.push_str(&"░".repeat(width - filled));
    if color() { format!("\x1b[36m{s}\x1b[0m") } else { s }
}

/// Render rows from [`tdy_scan::Index::tree`] with box-drawing guides.
pub fn print_tree(rows: &[TreeRow]) {
    // last-child flags: row i is last if the next row at depth <= d has depth < d
    let n = rows.len();
    let mut is_last = vec![true; n];
    for i in 0..n {
        let d = rows[i].depth;
        for r in rows.iter().skip(i + 1) {
            if r.depth < d {
                break;
            }
            if r.depth == d {
                is_last[i] = false;
                break;
            }
        }
    }
    let mut open: Vec<bool> = Vec::new(); // whether ancestor at depth k has more siblings below
    for (i, r) in rows.iter().enumerate() {
        open.truncate(r.depth);
        let mut prefix = String::new();
        if r.depth > 0 {
            for k in 1..r.depth {
                prefix.push_str(if open.get(k).copied().unwrap_or(false) { "│  " } else { "   " });
            }
            prefix.push_str(if is_last[i] { "└─ " } else { "├─ " });
        }
        while open.len() <= r.depth {
            open.push(false);
        }
        open[r.depth] = !is_last[i];
        let name = if r.is_dir && r.depth > 0 { bold(&r.name) } else { r.name.clone() };
        println!("{:>9} {:>4} {} {}{}", fmt::bytes(r.size), format!("{:.0}%", r.fraction * 100.0), bar(r.fraction, 12), dim(&prefix), name);
    }
}

fn paint(code: &str, s: &str) -> String {
    if color() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
