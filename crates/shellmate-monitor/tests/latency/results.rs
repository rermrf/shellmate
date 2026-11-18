use std::{collections::BTreeMap, time::Duration};

use serde_json::{Value, json};

pub type Sample = Result<Duration, &'static str>;
pub type Run = BTreeMap<&'static str, Sample>;

fn median_min(samples: &mut [Duration]) -> Option<(f64, f64)> {
    samples.sort_unstable();
    let min = samples.first()?.as_secs_f64() * 1000.0;
    let lower = samples[(samples.len() - 1) / 2].as_secs_f64();
    let upper = samples[samples.len() / 2].as_secs_f64();
    Some(((lower + upper) * 500.0, min))
}

pub fn report(target: &str, rtt_ms: u64, runs: &[Run]) -> Vec<Value> {
    let mut rows = Vec::new();
    for &metric in runs[0].keys() {
        let mut samples = Vec::new();
        let mut notes = Vec::new();
        for run in runs {
            match run[metric] {
                Ok(duration) => samples.push(duration),
                Err(note) if !notes.contains(&note) => notes.push(note),
                Err(_) => (),
            }
        }
        let (median_ms, min_ms) = median_min(&mut samples).unzip();
        let display = |value: Option<f64>| value.map_or_else(|| "-".into(), |ms| format!("{ms:.2}"));
        let measured = samples.len();
        let total = runs.len();
        let note = notes.join("; ");
        eprintln!(
            "{target:<8} {rtt_ms:>6}  {metric:<22} {:>11} {:>11}  {measured}/{total}  {note}",
            display(median_ms),
            display(min_ms)
        );
        rows.push(json!({ "target": target, "rtt_ms": rtt_ms, "metric": metric,
            "median_ms": median_ms, "min_ms": min_ms, "measured": measured, "runs": total, "note": note }));
    }
    rows
}

#[test]
fn summarizes_odd_even_and_skipped_samples_in_milliseconds() {
    let run = |sample| Run::from([("metric", sample)]);
    let ms = |value| run(Ok(Duration::from_millis(value)));
    let skip = || run(Err("unavailable"));
    let row = |runs: &[Run]| report("test", 250, runs).remove(0);
    assert_eq!(row(&[ms(10), ms(3), ms(5)])["median_ms"], 5.0);
    let even = row(&[ms(4), skip(), ms(1)]);
    assert_eq!(even["median_ms"], 2.5);
    assert_eq!(even["min_ms"], 1.0);
    assert_eq!(even["measured"], 2);
    assert_eq!(even["runs"], 3);
    let skipped = row(&[skip()]);
    assert!(skipped["min_ms"].is_null() && skipped["median_ms"].is_null());
    assert_eq!(skipped["note"], "unavailable");
}
