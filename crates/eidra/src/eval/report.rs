//! Progress lines, a run's report, and two runs compared.

use std::collections::BTreeMap;

use super::task::Split;
use super::{Attempt, Manifest};
use crate::format::{compact, duration, thousands};

/// A task's attempts, summed up.
struct Summary {
    split: Split,
    attempts: usize,
    passed: usize,
    /// Medians over the attempts.
    wall: f64,
    requests: f64,
    output: f64,
    /// Prompt tokens the cache supplied, over all attempts.
    cached_share: Option<f64>,
    /// Tool errors per attempt.
    errors: f64,
}

fn summarise(attempts: &[Attempt]) -> BTreeMap<&str, Summary> {
    let mut by_task: BTreeMap<&str, Vec<&Attempt>> = BTreeMap::new();
    for attempt in attempts {
        by_task.entry(attempt.task.as_str()).or_default().push(attempt);
    }
    by_task
        .into_iter()
        .map(|(task, list)| {
            let prompt: u64 = list.iter().map(|a| a.metrics.prompt_tokens).sum();
            let cached: u64 = list.iter().map(|a| a.metrics.cached_tokens).sum();
            let summary = Summary {
                split: list[0].split,
                attempts: list.len(),
                passed: list.iter().filter(|a| a.passed).count(),
                wall: median(list.iter().map(|a| a.wall_s)),
                requests: median(list.iter().map(|a| a.metrics.requests as f64)),
                output: median(list.iter().map(|a| a.metrics.output_tokens as f64)),
                cached_share: (prompt > 0).then(|| cached as f64 / prompt as f64),
                errors: list.iter().map(|a| a.metrics.tool_error_count() as f64).sum::<f64>()
                    / list.len() as f64,
            };
            (task, summary)
        })
        .collect()
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values: Vec<f64> = values.collect();
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 { values[mid] } else { (values[mid - 1] + values[mid]) / 2.0 }
}

/// Passed and attempted, for one split or all of them.
fn tally(attempts: &[Attempt], split: Option<Split>) -> (usize, usize) {
    let chosen: Vec<&Attempt> =
        attempts.iter().filter(|a| split.is_none_or(|s| a.split == s)).collect();
    (chosen.iter().filter(|a| a.passed).count(), chosen.len())
}

fn share(part: f64) -> String {
    format!("{:.0}%", 100.0 * part)
}

/// The start of a hash, enough to tell runs apart; runs from before binaries were hashed have none.
pub(super) fn short(hash: &str) -> &str {
    if hash.is_empty() { "-" } else { &hash[..8.min(hash.len())] }
}

pub fn progress(done: usize, total: usize, attempt: &Attempt) {
    if let Some(reason) = &attempt.lost {
        let width = total.to_string().len();
        let reason = reason.lines().next().unwrap_or_default();
        eprintln!(
            "[{done:>width$}/{total}] {:<16} #{:<2} LOST: {reason}",
            attempt.task, attempt.repeat
        );
        return;
    }
    let verdict = if attempt.passed {
        "pass".to_owned()
    } else if attempt.timed_out {
        "TIMED OUT".to_owned()
    } else {
        let failed: Vec<&str> =
            attempt.checks.iter().filter(|c| !c.passed).map(|c| c.name.as_str()).collect();
        format!("FAIL ({})", failed.join(", "))
    };
    let m = &attempt.metrics;
    let cached = if m.prompt_tokens > 0 {
        share(m.cached_tokens as f64 / m.prompt_tokens as f64)
    } else {
        "-".into()
    };
    let ended = match m.outcome.as_deref() {
        Some("finished") => String::new(),
        Some(other) => format!("  ended: {other}"),
        None => "  ended without finishing".into(),
    };
    let width = total.to_string().len();
    eprintln!(
        "[{done:>width$}/{total}] {:<16} #{:<2} {:<24} {:>8} {:>4} req {:>6} out {:>4} cached{ended}",
        attempt.task,
        attempt.repeat,
        verdict,
        duration(attempt.wall_s),
        m.requests,
        compact(m.output_tokens as f64),
        cached,
    );
}

/// The attempts that say something about the agent, and how many were lost.
fn counted(attempts: &[Attempt]) -> (Vec<Attempt>, usize) {
    let (lost, counted): (Vec<&Attempt>, Vec<&Attempt>) =
        attempts.iter().partition(|a| a.is_lost());
    (counted.into_iter().cloned().collect(), lost.len())
}

pub fn print(manifest: &Manifest, attempts: &[Attempt]) {
    let (attempts, lost) = counted(attempts);
    let attempts = attempts.as_slice();
    let regraded =
        manifest.regraded.as_ref().map_or(String::new(), |when| format!(" · regraded {when}"));
    let network = match manifest.network {
        Some(super::Network::Vacuum) => "vacuum",
        Some(super::Network::Host) | None => "host network",
    };
    println!(
        "{} · profile {} ({}) · binary {} · {network} · {} · started {}{regraded}",
        manifest.label,
        manifest.profile,
        short(&manifest.profile_hash),
        short(&manifest.binary_hash),
        manifest.model,
        manifest.started
    );
    println!(
        "  {:<16} {:<5} {:>6} {:>8} {:>8} {:>7} {:>6} {:>6}",
        "task", "split", "passed", "wall", "requests", "output", "cached", "errors"
    );
    for (task, s) in summarise(attempts) {
        println!(
            "  {:<16} {:<5} {:>6} {:>8} {:>8.0} {:>7} {:>6} {:>6.1}",
            task,
            s.split.name(),
            format!("{}/{}", s.passed, s.attempts),
            duration(s.wall),
            s.requests,
            compact(s.output),
            s.cached_share.map_or("-".into(), share),
            s.errors,
        );
    }
    println!();
    for (name, split) in [("train", Some(Split::Train)), ("test", Some(Split::Test)), ("all", None)]
    {
        let (passed, total) = tally(attempts, split);
        if total > 0 {
            println!(
                "  {name:<5} {passed}/{total} passed ({})",
                share(passed as f64 / total as f64)
            );
        }
    }
    let wall: f64 = attempts.iter().map(|a| a.wall_s).sum();
    let output: u64 = attempts.iter().map(|a| a.metrics.output_tokens).sum();
    let computed: u64 = attempts.iter().map(|a| a.metrics.computed_tokens).sum();
    println!(
        "  agent time {} · {} tokens generated · {} prompt tokens computed",
        duration(wall),
        thousands(output),
        thousands(computed)
    );
    println!(
        "  medians per attempt; cached is the prompt share the cache supplied; errors are tool errors per attempt"
    );
    if lost > 0 {
        println!(
            "  {lost} attempts lost to something outside the agent are left out; `eidra eval run \
             --resume` runs them again"
        );
    }
}

pub fn compare(before: &(Manifest, Vec<Attempt>), after: &(Manifest, Vec<Attempt>)) {
    let (before_manifest, before_attempts) = before;
    let (after_manifest, after_attempts) = after;
    let (before_attempts, after_attempts) = (counted(before_attempts).0, counted(after_attempts).0);
    let (before_attempts, after_attempts) = (before_attempts.as_slice(), after_attempts.as_slice());
    let name = |m: &Manifest| {
        format!(
            "{} (profile {}, binary {}, {})",
            m.label,
            short(&m.profile_hash),
            short(&m.binary_hash),
            m.started
        )
    };
    println!("before: {}\nafter:  {}", name(before_manifest), name(after_manifest));
    let old = summarise(before_attempts);
    let new = summarise(after_attempts);
    println!(
        "  {:<16} {:<5} {:>13} {:>17} {:>11} {:>15} {:>11}",
        "task", "split", "passed", "wall", "requests", "output", "errors"
    );
    for (task, b) in &old {
        let Some(a) = new.get(task) else {
            continue;
        };
        println!(
            "  {:<16} {:<5} {:>13} {:>17} {:>11} {:>15} {:>11}",
            task,
            b.split.name(),
            format!("{}/{} → {}/{}", b.passed, b.attempts, a.passed, a.attempts),
            format!("{} → {}", duration(b.wall), duration(a.wall)),
            format!("{:.0} → {:.0}", b.requests, a.requests),
            format!("{} → {}", compact(b.output), compact(a.output)),
            format!("{:.1} → {:.1}", b.errors, a.errors),
        );
    }
    // Totals over the tasks both runs have.
    let shared = |attempts: &[Attempt]| -> Vec<Attempt> {
        attempts
            .iter()
            .filter(|a| old.contains_key(a.task.as_str()) && new.contains_key(a.task.as_str()))
            .cloned()
            .collect()
    };
    let (b, a) = (shared(before_attempts), shared(after_attempts));
    println!();
    for (label, split) in
        [("train", Some(Split::Train)), ("test", Some(Split::Test)), ("all", None)]
    {
        let ((bp, bt), (ap, at)) = (tally(&b, split), tally(&a, split));
        if bt + at > 0 {
            println!("  {label:<5} {bp}/{bt} → {ap}/{at} passed");
        }
    }
    let total = |attempts: &[Attempt], f: fn(&Attempt) -> f64| attempts.iter().map(f).sum::<f64>();
    println!(
        "  agent time {} → {} · tokens generated {} → {}",
        duration(total(&b, |x| x.wall_s)),
        duration(total(&a, |x| x.wall_s)),
        compact(total(&b, |x| x.metrics.output_tokens as f64)),
        compact(total(&a, |x| x.metrics.output_tokens as f64)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians_and_tallies() {
        assert_eq!(median([3.0, 1.0, 2.0].into_iter()), 2.0);
        assert_eq!(median([4.0, 1.0, 2.0, 3.0].into_iter()), 2.5);
        assert_eq!(median(std::iter::empty()), 0.0);
    }
}
