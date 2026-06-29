use anyhow::Result;
use seda_sdk_rs::{elog, get_reveals, log, Process};
use serde::Serialize;

use crate::execution_phase::ExecutionResult;

/// Session-Aware Equity Collateral Oracle — Tally Phase
///
/// Computes a continuous, manipulation-resistant mark price for tokenized equities.
///
/// Three regimes:
///   OPEN:       Median of live equity sources with MAD outlier rejection.
///   CLOSED/PRE/POST: Self-referential EMA from last in-session reference,
///                     updated by still-moving signals. Thin off-hours prints
///                     cannot move the mark materially.
///   TRANSITION: Smooth interpolation from off-hours EMA to live composite
///               over a configurable window (prevents reopening-gap cascade).
///
/// Variance guard: if mark deviates beyond threshold from reference, flag as DEFER.

#[derive(Serialize)]
struct MarkOutput {
    /// The collateral mark price (micro-cents)
    mark: u64,
    /// Mark as human-readable USD
    mf: String,
    /// Session state
    ss: String,
    /// Is the mark valid? false = DEFER (variance guard triggered)
    valid: bool,
    /// Confidence 0-100
    conf: u8,
    /// Sources used (after filter)
    su: usize,
    /// Source names
    sn: Vec<String>,
    /// Symbol
    sy: String,
    /// Executors
    ex: usize,
    /// Valid reveals
    ok: usize,
}

fn median_u64(vals: &mut Vec<u64>) -> u64 {
    if vals.is_empty() { return 0; }
    vals.sort();
    let len = vals.len();
    if len % 2 == 0 { (vals[len / 2 - 1] + vals[len / 2]) / 2 } else { vals[len / 2] }
}

/// Median Absolute Deviation
fn mad(values: &[u64], med: u64) -> u64 {
    if values.is_empty() { return 0; }
    let mut devs: Vec<u64> = values.iter()
        .map(|v| if *v > med { *v - med } else { med - *v })
        .collect();
    devs.sort();
    let len = devs.len();
    if len % 2 == 0 { (devs[len / 2 - 1] + devs[len / 2]) / 2 } else { devs[len / 2] }
}

/// Compute EMA-adjusted mark for off-hours
/// reference = last in-session mark, signal = current off-hours price
/// alpha = 2 / (period + 1) — lower alpha = more resistant to manipulation
fn ema_mark(reference: u64, signal: u64, period: u32) -> u64 {
    if reference == 0 { return signal; }
    if signal == 0 { return reference; }

    let alpha = 2.0 / (period as f64 + 1.0);
    let ema = alpha * signal as f64 + (1.0 - alpha) * reference as f64;
    ema.round() as u64
}

/// Interpolate from off-hours mark to live mark over transition window
/// progress: 0.0 = window start (use off-hours mark), 1.0 = window end (use live mark)
fn transition_interpolate(off_hours_mark: u64, live_mark: u64, progress: f64) -> u64 {
    let p = progress.max(0.0).min(1.0);
    // Smooth easing: cubic ease-in-out for natural feel
    let t = if p < 0.5 {
        4.0 * p * p * p
    } else {
        1.0 - (-2.0 * p + 2.0).powi(3) / 2.0
    };
    let result = off_hours_mark as f64 * (1.0 - t) + live_mark as f64 * t;
    result.round() as u64
}

/// Dispersion in basis points
fn dispersion_bps(values: &[u64], center: u64) -> u64 {
    if values.is_empty() || center == 0 { return 0; }
    let min_v = *values.iter().min().unwrap();
    let max_v = *values.iter().max().unwrap();
    let spread = max_v - min_v;
    (spread as u128 * 10000 / center as u128) as u64
}

pub fn tally_phase() -> Result<()> {
    let reveals = get_reveals()?;
    let num_executors = reveals.len();

    log!("Equity collateral tally: {} reveals", num_executors);

    let mut results: Vec<ExecutionResult> = Vec::new();
    for reveal in reveals {
        match serde_json::from_slice::<ExecutionResult>(&reveal.body.reveal) {
            Ok(r) => results.push(r),
            Err(e) => { elog!("Parse error: {}", e); }
        }
    }

    if results.is_empty() {
        Process::error(b"No valid reveals");
    }

    let num_valid = results.len();
    let ref_result = &results[0];
    let symbol = ref_result.sy.clone();
    let session_state = ref_result.ss.clone();
    let variance_threshold = ref_result.vt;
    let ema_period = ref_result.ep;
    let transition_window = ref_result.tw;
    let reference_price = ref_result.rp;
    let secs_since_open = ref_result.so;

    // ── 1. Collect all valid source prices across executors ──────────
    let mut source_map: std::collections::BTreeMap<String, Vec<u64>> = std::collections::BTreeMap::new();
    for result in &results {
        for src in &result.src {
            if src.ok && src.p > 0 {
                source_map.entry(src.n.clone()).or_default().push(src.p);
            }
        }
    }

    // Per-source median (executor-level resistance)
    let mut source_medians: Vec<(String, u64)> = Vec::new();
    for (name, prices) in &mut source_map {
        prices.sort();
        let med = median_u64(prices);
        if med > 0 {
            source_medians.push((name.clone(), med));
        }
    }

    log!("{} sources with valid prices", source_medians.len());

    // ── 2. Cross-source median + MAD outlier rejection ──────────────
    let mut cross_prices: Vec<u64> = source_medians.iter().map(|(_, p)| *p).collect();
    let live_median = if cross_prices.is_empty() { 0 } else {
        cross_prices.sort();
        let med = median_u64(&mut cross_prices.clone());

        // MAD filter
        let mad_val = mad(&cross_prices, med);
        let mad_threshold = mad_val * 2;

        let mut filtered: Vec<(String, u64)> = Vec::new();
        for (name, price) in &source_medians {
            let dev = if *price > med { *price - med } else { med - *price };
            if mad_threshold > 0 && dev > mad_threshold {
                log!("REJECT {}: ${:.2} (outlier)", name, *price as f64 / 1_000_000.0);
            } else {
                filtered.push((name.clone(), *price));
            }
        }

        if filtered.is_empty() {
            // All rejected — use full set
            filtered = source_medians.clone();
        }

        source_medians = filtered;
        let mut fp: Vec<u64> = source_medians.iter().map(|(_, p)| *p).collect();
        median_u64(&mut fp)
    };

    let source_names: Vec<String> = source_medians.iter().map(|(n, _)| n.clone()).collect();
    let source_count = source_medians.len();

    // ── 3. Compute mark based on session state ──────────────────────
    let (mark, confidence) = match session_state.as_str() {
        "OPEN" => {
            if live_median == 0 {
                log!("OPEN but no live prices — using reference");
                (reference_price, 30u8)
            } else {
                log!("OPEN: live median ${:.2}", live_median as f64 / 1_000_000.0);
                (live_median, 95)
            }
        }

        "TRANSITION" => {
            // Interpolate from off-hours EMA to live composite
            let off_hours_mark = if reference_price > 0 && live_median > 0 {
                ema_mark(reference_price, live_median, ema_period)
            } else if reference_price > 0 {
                reference_price
            } else {
                live_median
            };

            let progress = if transition_window > 0 {
                secs_since_open as f64 / transition_window as f64
            } else {
                1.0
            };

            if live_median > 0 {
                let mark = transition_interpolate(off_hours_mark, live_median, progress);
                log!("TRANSITION: {:.0}% through window, mark ${:.2}",
                    progress * 100.0, mark as f64 / 1_000_000.0);
                (mark, (60.0 + progress * 35.0).round() as u8)
            } else {
                log!("TRANSITION: no live data, using EMA");
                (off_hours_mark, 40)
            }
        }

        // CLOSED_WEEKEND, CLOSED_HOLIDAY, PRE, POST
        _ => {
            // Off-hours: EMA from reference, updated by any available signal
            if reference_price == 0 && live_median == 0 {
                log!("{}: no reference and no live data", session_state);
                Process::error(b"No reference price and no live data for off-hours mark");
            }

            let signal = if live_median > 0 { live_median } else { reference_price };
            let mark = ema_mark(reference_price, signal, ema_period);

            let conf = if live_median > 0 { 60u8 } else { 40 };
            log!("{}: EMA mark ${:.2} (ref=${:.2}, signal=${:.2}, period={})",
                session_state,
                mark as f64 / 1_000_000.0,
                reference_price as f64 / 1_000_000.0,
                signal as f64 / 1_000_000.0,
                ema_period);
            (mark, conf)
        }
    };

    // ── 4. Variance guard ───────────────────────────────────────────
    let valid = if reference_price > 0 && mark > 0 {
        let dev = if mark > reference_price {
            mark - reference_price
        } else {
            reference_price - mark
        };
        let dev_bps = (dev as u128 * 10000 / reference_price as u128) as u64;

        if dev_bps > variance_threshold {
            log!("DEFER: mark deviation {}bps exceeds threshold {}bps",
                dev_bps, variance_threshold);
            false
        } else {
            true
        }
    } else {
        true // no reference to compare against
    };

    // ── 5. Emit result ──────────────────────────────────────────────
    let mf = format!("{:.2}", mark as f64 / 1_000_000.0);

    let output = MarkOutput {
        mark,
        mf,
        ss: session_state,
        valid,
        conf: confidence,
        su: source_count,
        sn: source_names,
        sy: symbol,
        ex: num_executors,
        ok: num_valid,
    };

    let json_bytes = serde_json::to_vec(&output)?;
    log!("Mark: ${} [{}] valid={} conf={}", output.mf, output.ss, output.valid, output.conf);
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
