use anyhow::Result;
use seda_sdk_rs::{elog, get_reveals, log, Process};
use serde::Serialize;

use crate::execution_phase::{ExecutionResult, RedemptionState, SourcePrice};

/// Session-Aware Equity Collateral Oracle — Tally Phase (v2)
///
/// Computes REALIZABLE collateral value — what a liquidator can actually
/// sell for right now — not just a reference price.
///
/// Three-signal triangulation:
///   1. Underlying reference (live equity, in-session only)
///   2. Tokenized secondary (what the token actually trades for, 24/7)
///   3. NAV / redemption (only trustworthy if redemption is accessible)
///
/// Anchor mode:
///   NAV_ANCHORED — redemption open + instant + accessible + liquid → mark near NAV
///   SECONDARY_REALIZABLE — otherwise → mark = secondary market (what you can sell for)
///
/// Depth discount: thin books → discounted mark + widened confidence.

#[derive(Serialize)]
struct MarkOutput {
    /// Realizable collateral value (micro-cents)
    mark: u64,
    /// Human-readable USD
    mf: String,
    /// Anchor mode: "NAV_ANCHORED" or "SECONDARY_REALIZABLE"
    anchor: String,
    /// Session state
    ss: String,
    /// Is the mark valid? false = DEFER
    valid: bool,
    /// Confidence 0-100
    conf: u8,
    /// Can a liquidator redeem at NAV?
    ra: bool,
    /// Underlying reference (micro-cents, 0 if unavailable)
    ul: u64,
    /// Secondary market price (micro-cents, 0 if unavailable)
    sec: u64,
    /// NAV price (micro-cents, 0 if unknown)
    nav: u64,
    /// Depth discount applied (basis points, 0 = no discount)
    dd: u64,
    /// Sources used
    su: usize,
    /// Source names
    sn: Vec<String>,
    /// Symbol
    sy: String,
    /// Executors
    ex: usize,
    ok: usize,
}

fn median_u64(vals: &mut Vec<u64>) -> u64 {
    if vals.is_empty() { return 0; }
    vals.sort();
    let len = vals.len();
    if len % 2 == 0 { (vals[len / 2 - 1] + vals[len / 2]) / 2 } else { vals[len / 2] }
}

fn mad(values: &[u64], med: u64) -> u64 {
    if values.is_empty() { return 0; }
    let mut devs: Vec<u64> = values.iter()
        .map(|v| if *v > med { *v - med } else { med - *v })
        .collect();
    devs.sort();
    let len = devs.len();
    if len % 2 == 0 { (devs[len / 2 - 1] + devs[len / 2]) / 2 } else { devs[len / 2] }
}

/// Off-hours composite: reference-anchored weighted average of all available signals.
/// Reference gets weight = period, each live signal gets weight = 1.
/// Higher period → more anchored to reference → more manipulation-resistant.
/// Unlike EMA, this is stateless — same inputs always produce the same output.
fn composite_mark(reference: u64, signals: &[u64], period: u32) -> u64 {
    if reference == 0 && signals.is_empty() { return 0; }
    if signals.is_empty() { return reference; }
    if reference == 0 {
        return signals.iter().sum::<u64>() / signals.len() as u64;
    }

    // Reference anchors with weight = period, each signal gets weight = 1
    let ref_weight = period as f64;
    let total_weight = ref_weight + signals.len() as f64;
    let weighted_sum = reference as f64 * ref_weight
        + signals.iter().map(|&s| s as f64).sum::<f64>();
    (weighted_sum / total_weight).round() as u64
}

fn transition_interpolate(off_hours: u64, live: u64, progress: f64) -> u64 {
    let p = progress.max(0.0).min(1.0);
    let t = if p < 0.5 { 4.0 * p * p * p } else { 1.0 - (-2.0 * p + 2.0).powi(3) / 2.0 };
    (off_hours as f64 * (1.0 - t) + live as f64 * t).round() as u64
}

/// Is redemption genuinely accessible to an anonymous liquidator?
fn is_redemption_accessible(rd: &RedemptionState) -> bool {
    rd.open && rd.instant && rd.accessible_to_liquidator && rd.swapper_liquidity_sufficient
}

/// Compute median of valid source prices across all executor reveals
fn compute_signal_median(
    results: &[ExecutionResult],
    extract: impl Fn(&ExecutionResult) -> Vec<&SourcePrice>,
) -> (u64, Vec<String>, usize) {
    let mut all_prices: Vec<u64> = Vec::new();
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for result in results {
        for src in extract(result) {
            if src.ok && src.p > 0 {
                all_prices.push(src.p);
                names.insert(src.n.clone());
            }
        }
    }

    if all_prices.is_empty() {
        return (0, Vec::new(), 0);
    }

    // MAD outlier rejection
    all_prices.sort();
    let med = median_u64(&mut all_prices.clone());
    let mad_val = mad(&all_prices, med);
    let threshold = mad_val * 2;

    let mut filtered: Vec<u64> = if threshold > 0 {
        all_prices.iter().filter(|&&p| {
            let dev = if p > med { p - med } else { med - p };
            dev <= threshold
        }).copied().collect()
    } else {
        all_prices
    };

    if filtered.is_empty() {
        filtered = vec![med];
    }

    let final_med = median_u64(&mut filtered);
    let count = names.len();
    (final_med, names.into_iter().collect(), count)
}

/// Apply depth discount: thin books → discounted mark
/// Returns (discounted_price, discount_bps)
fn apply_depth_discount(price: u64, depth_usd: u64, threshold_usd: u64) -> (u64, u64) {
    if depth_usd == 0 || threshold_usd == 0 {
        return (price, 0);
    }
    if depth_usd >= threshold_usd {
        return (price, 0); // sufficient depth, no discount
    }

    // Linear discount: 0% at threshold, max 5% at zero depth
    let ratio = depth_usd as f64 / threshold_usd as f64; // 0.0 to 1.0
    let discount_bps = ((1.0 - ratio) * 500.0).round() as u64; // max 500bps = 5%
    let discounted = (price as f64 * (1.0 - discount_bps as f64 / 10000.0)).round() as u64;

    (discounted, discount_bps)
}

pub fn tally_phase() -> Result<()> {
    let reveals = get_reveals()?;
    let num_executors = reveals.len();
    log!("Equity collateral tally v2: {} reveals", num_executors);

    let mut results: Vec<ExecutionResult> = Vec::new();
    for reveal in reveals {
        match serde_json::from_slice::<ExecutionResult>(&reveal.body.reveal) {
            Ok(r) => results.push(r),
            Err(e) => { elog!("Parse error: {}", e); }
        }
    }
    if results.is_empty() { Process::error(b"No valid reveals"); }

    let num_valid = results.len();
    let ref_r = &results[0];
    let symbol = ref_r.sy.clone();
    let session = ref_r.ss.clone();
    let variance_threshold = ref_r.vt;
    let ema_period = ref_r.ep;
    let transition_window = ref_r.tw;
    let reference_price = ref_r.rp;
    let secs_since_open = ref_r.so;
    let redemption = ref_r.rd.clone();
    let depth_usd = ref_r.dp;
    let depth_threshold = ref_r.dt;

    // ── 1. Compute three signal medians ─────────────────────────────

    // Signal 1: Underlying reference
    let (underlying_med, ul_names, ul_count) = compute_signal_median(&results, |r| {
        r.underlying.iter().collect()
    });
    if underlying_med > 0 {
        log!("Underlying: ${:.2} ({} sources)", underlying_med as f64 / 1_000_000.0, ul_count);
    }

    // Signal 2: Secondary market
    let (secondary_med, sec_names, sec_count) = compute_signal_median(&results, |r| {
        r.secondary.iter().collect()
    });
    if secondary_med > 0 {
        log!("Secondary: ${:.2} ({} venues)", secondary_med as f64 / 1_000_000.0, sec_count);
    }

    // Pyth (still-moving proxy for off-hours)
    let mut pyth_prices: Vec<u64> = results.iter()
        .filter(|r| r.pyth.ok && r.pyth.p > 0)
        .map(|r| r.pyth.p)
        .collect();
    let pyth_med = if pyth_prices.is_empty() { 0 } else { median_u64(&mut pyth_prices) };

    // Signal 3: NAV
    let nav = results.iter().map(|r| r.nav).find(|&n| n > 0).unwrap_or(0);
    if nav > 0 {
        log!("NAV: ${:.2}", nav as f64 / 1_000_000.0);
    }

    // All source names for output
    let mut all_names: Vec<String> = Vec::new();
    all_names.extend(ul_names);
    all_names.extend(sec_names);
    if pyth_med > 0 { all_names.push("Pyth".into()); }
    let source_count = all_names.len();

    // ── 2. Determine anchor mode ────────────────────────────────────

    let redemption_accessible = is_redemption_accessible(&redemption);
    let anchor_mode = if redemption_accessible && nav > 0 {
        "NAV_ANCHORED"
    } else {
        "SECONDARY_REALIZABLE"
    };
    log!("Anchor: {} (redemption accessible: {})", anchor_mode, redemption_accessible);

    // ── 3. Compute raw mark based on session + anchor ───────────────

    let raw_mark = match session.as_str() {
        "OPEN" => {
            // In-session: use underlying if available
            if underlying_med > 0 {
                if anchor_mode == "NAV_ANCHORED" && nav > 0 {
                    // Anchor near NAV, bounded by underlying
                    let avg = (underlying_med + nav) / 2;
                    log!("OPEN NAV_ANCHORED: avg(underlying, NAV) = ${:.2}", avg as f64 / 1_000_000.0);
                    avg
                } else if secondary_med > 0 {
                    // SECONDARY_REALIZABLE: use the lower of underlying and secondary
                    // (conservative — liquidator gets the worse price)
                    let realizable = underlying_med.min(secondary_med);
                    log!("OPEN SECONDARY: min(underlying, secondary) = ${:.2}", realizable as f64 / 1_000_000.0);
                    realizable
                } else {
                    underlying_med
                }
            } else if secondary_med > 0 {
                secondary_med
            } else {
                reference_price
            }
        }

        "TRANSITION" => {
            let off_hours_signals: Vec<u64> = [secondary_med, pyth_med].iter()
                .filter(|&&p| p > 0).copied().collect();
            let off_hours_mark = composite_mark(reference_price, &off_hours_signals, ema_period);

            let live = if underlying_med > 0 { underlying_med }
                       else if secondary_med > 0 { secondary_med }
                       else { off_hours_mark };

            let progress = if transition_window > 0 { secs_since_open as f64 / transition_window as f64 } else { 1.0 };
            let mark = transition_interpolate(off_hours_mark, live, progress);
            log!("TRANSITION: {:.0}% → ${:.2}", progress * 100.0, mark as f64 / 1_000_000.0);
            mark
        }

        // CLOSED_WEEKEND, CLOSED_HOLIDAY, PRE, POST
        _ => {
            // Off-hours: reference-anchored composite of secondary + pyth
            // NOT single-venue, NOT EMA (stateless — same inputs = same output)
            let mut signals: Vec<u64> = Vec::new();
            if secondary_med > 0 { signals.push(secondary_med); }
            if pyth_med > 0 { signals.push(pyth_med); }

            if signals.is_empty() && reference_price == 0 {
                Process::error(b"No signals and no reference for off-hours mark");
            }

            let mark = composite_mark(reference_price, &signals, ema_period);
            log!("{}: composite ${:.2} (ref=${:.2}, {} signals, period={})",
                session, mark as f64 / 1_000_000.0,
                reference_price as f64 / 1_000_000.0, signals.len(), ema_period);
            mark
        }
    };

    // ── 4. Apply depth discount ─────────────────────────────────────

    let (depth_adjusted_mark, depth_discount_bps) = if anchor_mode == "SECONDARY_REALIZABLE" {
        apply_depth_discount(raw_mark, depth_usd, depth_threshold)
    } else {
        (raw_mark, 0u64)
    };

    if depth_discount_bps > 0 {
        log!("Depth discount: {}bps (depth ${}K / threshold ${}K)",
            depth_discount_bps, depth_usd / 1000, depth_threshold / 1000);
    }

    // ── 5. Confidence scoring ───────────────────────────────────────

    let mut confidence: u8 = match session.as_str() {
        "OPEN" => 95,
        "TRANSITION" => {
            let progress = if transition_window > 0 { secs_since_open as f64 / transition_window as f64 } else { 1.0 };
            (60.0 + progress * 35.0).round() as u8
        }
        _ => if secondary_med > 0 && pyth_med > 0 { 60 } else if secondary_med > 0 || pyth_med > 0 { 40 } else { 20 },
    };

    // Widen confidence for thin depth
    if depth_discount_bps > 0 {
        let penalty = (depth_discount_bps as f64 / 500.0 * 20.0).round() as u8;
        confidence = confidence.saturating_sub(penalty);
    }

    // Lower confidence if redemption inaccessible and secondary is thin
    if !redemption_accessible && sec_count <= 1 {
        confidence = confidence.saturating_sub(10);
    }

    // ── 6. Variance guard ───────────────────────────────────────────

    let valid = if reference_price > 0 && depth_adjusted_mark > 0 {
        let dev = if depth_adjusted_mark > reference_price {
            depth_adjusted_mark - reference_price
        } else {
            reference_price - depth_adjusted_mark
        };
        let dev_bps = (dev as u128 * 10000 / reference_price as u128) as u64;
        if dev_bps > variance_threshold {
            log!("DEFER: {}bps deviation > {}bps threshold", dev_bps, variance_threshold);
            false
        } else { true }
    } else { true };

    // ── 7. Emit ─────────────────────────────────────────────────────

    let mf = format!("{:.2}", depth_adjusted_mark as f64 / 1_000_000.0);

    let output = MarkOutput {
        mark: depth_adjusted_mark,
        mf,
        anchor: anchor_mode.to_string(),
        ss: session,
        valid,
        conf: confidence,
        ra: redemption_accessible,
        ul: underlying_med,
        sec: secondary_med,
        nav,
        dd: depth_discount_bps,
        su: source_count,
        sn: all_names,
        sy: symbol,
        ex: num_executors,
        ok: num_valid,
    };

    let json_bytes = serde_json::to_vec(&output)?;
    log!("Mark: ${} [{}] {} valid={} conf={}", output.mf, output.ss, output.anchor, output.valid, output.conf);
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
