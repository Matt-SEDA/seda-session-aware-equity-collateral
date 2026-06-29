use anyhow::Result;
use seda_sdk_rs::{http_fetch, log, Process};
use serde::{Deserialize, Serialize};

/// Session-Aware Equity Collateral Oracle — Execution Phase
///
/// Fetches equity price from multiple approved sources and determines
/// session state deterministically from the input timestamp.
///
/// Sources:
///   1. dxFeed (equity quotes, 15-min delayed on free tier)
///   2. Finage (real-time equity data)
///   3. Pyth (crypto/equity where available)
///
/// Session state is computed deterministically from timestamp + config.
/// No Date.now() — the input timestamp IS the program's clock.

const DXFEED_API: &str = "https://tools.dxfeed.com/webservice/rest";
const FINAGE_API: &str = "https://api.finage.co.uk/last/stock";
const PYTH_HERMES: &str = "https://hermes.pyth.network/v2/updates/price/latest";

// ── Session state ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub enum SessionState {
    Open,
    Pre,
    Post,
    ClosedWeekend,
    ClosedHoliday,
    Transition,
}

impl SessionState {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Open => "OPEN",
            Self::Pre => "PRE",
            Self::Post => "POST",
            Self::ClosedWeekend => "CLOSED_WEEKEND",
            Self::ClosedHoliday => "CLOSED_HOLIDAY",
            Self::Transition => "TRANSITION",
        }
    }
}

// ── Input ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OracleInput {
    /// Equity symbol, e.g. "NVDA"
    pub symbol: String,
    /// Reference timestamp (unix seconds) — the program's clock
    pub timestamp: u64,
    /// Session config
    #[serde(default)]
    pub config: SessionConfig,
}

#[derive(Deserialize)]
pub struct SessionConfig {
    /// Market open hour UTC (default 13:30 = NYSE open)
    #[serde(default = "default_open_hour")]
    pub open_hour: u32,
    #[serde(default = "default_open_min")]
    pub open_min: u32,
    /// Market close hour UTC (default 20:00 = NYSE close)
    #[serde(default = "default_close_hour")]
    pub close_hour: u32,
    #[serde(default = "default_close_min")]
    pub close_min: u32,
    /// Trading days (0=Sun, 1=Mon, ..., 6=Sat). Default Mon-Fri.
    #[serde(default = "default_trading_days")]
    pub trading_days: Vec<u32>,
    /// Holiday dates as "YYYY-MM-DD" strings
    #[serde(default)]
    pub holidays: Vec<String>,
    /// EMA period for off-hours mark (higher = more manipulation-resistant)
    #[serde(default = "default_ema_period")]
    pub ema_period: u32,
    /// Variance threshold in basis points (default 100 = 1%)
    #[serde(default = "default_variance_bps")]
    pub variance_threshold_bps: u64,
    /// Transition window in seconds after market open (default 900 = 15 min)
    #[serde(default = "default_transition_secs")]
    pub transition_secs: u64,
    /// Last known in-session reference price (micro-cents, for off-hours EMA seed)
    #[serde(default)]
    pub last_reference_price: u64,
    /// Pyth feed ID (optional, for equities that have Pyth feeds)
    #[serde(default)]
    pub pyth_feed_id: String,
}

fn default_open_hour() -> u32 { 13 }
fn default_open_min() -> u32 { 30 }
fn default_close_hour() -> u32 { 20 }
fn default_close_min() -> u32 { 0 }
fn default_trading_days() -> Vec<u32> { vec![1, 2, 3, 4, 5] } // Mon-Fri
fn default_ema_period() -> u32 { 20 }
fn default_variance_bps() -> u64 { 100 } // 1%
fn default_transition_secs() -> u64 { 900 } // 15 minutes

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            open_hour: default_open_hour(),
            open_min: default_open_min(),
            close_hour: default_close_hour(),
            close_min: default_close_min(),
            trading_days: default_trading_days(),
            holidays: Vec::new(),
            ema_period: default_ema_period(),
            variance_threshold_bps: default_variance_bps(),
            transition_secs: default_transition_secs(),
            last_reference_price: 0,
            pyth_feed_id: String::new(),
        }
    }
}

// ── Source price ─────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct SourcePrice {
    pub n: String,  // source name
    pub p: u64,     // price in micro-cents
    pub ok: bool,
}

// ── Execution result ────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Symbol
    pub sy: String,
    /// Source prices
    pub src: Vec<SourcePrice>,
    /// Session state
    pub ss: String,
    /// Input timestamp
    pub ts: u64,
    /// EMA period
    pub ep: u32,
    /// Variance threshold bps
    pub vt: u64,
    /// Transition window secs
    pub tw: u64,
    /// Last reference price (micro-cents, for off-hours EMA)
    pub rp: u64,
    /// Seconds since market open (for transition calc, 0 if not in transition)
    pub so: u64,
}

// ── Session detection (deterministic from timestamp) ────────────────

fn determine_session(ts: u64, config: &SessionConfig) -> (SessionState, u64) {
    // Convert unix timestamp to day-of-week and time-of-day
    // Unix epoch (1970-01-01) was a Thursday (day 4)
    let days_since_epoch = ts / 86400;
    let day_of_week = ((days_since_epoch + 4) % 7) as u32; // 0=Sun, 1=Mon, ..., 6=Sat

    let seconds_in_day = ts % 86400;
    let hour = (seconds_in_day / 3600) as u32;
    let minute = ((seconds_in_day % 3600) / 60) as u32;
    let time_minutes = hour * 60 + minute;

    let open_minutes = config.open_hour * 60 + config.open_min;
    let close_minutes = config.close_hour * 60 + config.close_min;

    // Check if today is a trading day
    let is_trading_day = config.trading_days.contains(&day_of_week);

    // Check holidays (compare date string)
    // Simple date computation from timestamp
    let date_str = unix_to_date_str(ts);
    let is_holiday = config.holidays.iter().any(|h| h == &date_str);

    if !is_trading_day {
        return (SessionState::ClosedWeekend, 0);
    }

    if is_holiday {
        return (SessionState::ClosedHoliday, 0);
    }

    if time_minutes < open_minutes {
        return (SessionState::Pre, 0);
    }

    if time_minutes >= close_minutes {
        return (SessionState::Post, 0);
    }

    // Market is open — check if we're in the transition window
    let secs_since_open = (time_minutes - open_minutes) as u64 * 60
        + (seconds_in_day % 60); // add remaining seconds

    if secs_since_open < config.transition_secs {
        return (SessionState::Transition, secs_since_open);
    }

    (SessionState::Open, secs_since_open)
}

/// Convert unix timestamp to "YYYY-MM-DD" string (deterministic)
fn unix_to_date_str(ts: u64) -> String {
    let days = (ts / 86400) as i64;
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

// ── Data fetching ───────────────────────────────────────────────────

fn fetch_dxfeed(symbol: &str) -> SourcePrice {
    let url = format!(
        "{}/events.json?events=Quote&symbols={}",
        DXFEED_API, symbol
    );
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        log!("dxFeed fetch failed");
        return SourcePrice { n: "dxFeed".into(), p: 0, ok: false };
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return SourcePrice { n: "dxFeed".into(), p: 0, ok: false },
    };

    // dxFeed response: { "Quote": { "NVDA": { "bidPrice": ..., "askPrice": ... } } }
    // Or array format: { "Quote": [{ "eventSymbol": "NVDA", "bidPrice": ..., "askPrice": ... }] }
    let quote = &v["Quote"];

    // Try object access first
    let bid = quote[symbol]["bidPrice"].as_f64()
        .or_else(|| quote[symbol]["lastPrice"].as_f64());
    let ask = quote[symbol]["askPrice"].as_f64();

    let price = match (bid, ask) {
        (Some(b), Some(a)) if b > 0.0 && a > 0.0 => (b + a) / 2.0,
        (Some(b), _) if b > 0.0 => b,
        (_, Some(a)) if a > 0.0 => a,
        _ => {
            // Try array format
            if let Some(arr) = quote.as_array() {
                for item in arr {
                    if item["eventSymbol"].as_str() == Some(symbol) {
                        let b = item["bidPrice"].as_f64().unwrap_or(0.0);
                        let a = item["askPrice"].as_f64().unwrap_or(0.0);
                        if b > 0.0 && a > 0.0 {
                            let mid = (b + a) / 2.0;
                            log!("dxFeed: ${:.2}", mid);
                            return SourcePrice { n: "dxFeed".into(), p: (mid * 1_000_000.0).round() as u64, ok: true };
                        }
                    }
                }
            }
            log!("dxFeed: no valid price");
            return SourcePrice { n: "dxFeed".into(), p: 0, ok: false };
        }
    };

    log!("dxFeed: ${:.2}", price);
    SourcePrice { n: "dxFeed".into(), p: (price * 1_000_000.0).round() as u64, ok: true }
}

fn fetch_finage(symbol: &str) -> SourcePrice {
    // Finage free tier — public delayed data
    let url = format!("{}/{}?apikey=demo", FINAGE_API, symbol);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        log!("Finage fetch failed");
        return SourcePrice { n: "Finage".into(), p: 0, ok: false };
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return SourcePrice { n: "Finage".into(), p: 0, ok: false },
    };

    // Finage: { "symbol": "NVDA", "ask": 130.5, "bid": 130.4, "asize": 1, "bsize": 2, "timestamp": ... }
    let bid = v["bid"].as_f64().unwrap_or(0.0);
    let ask = v["ask"].as_f64().unwrap_or(0.0);
    let last = v["last"].as_f64().or_else(|| v["price"].as_f64()).unwrap_or(0.0);

    let price = if bid > 0.0 && ask > 0.0 {
        (bid + ask) / 2.0
    } else if last > 0.0 {
        last
    } else {
        log!("Finage: no valid price");
        return SourcePrice { n: "Finage".into(), p: 0, ok: false };
    };

    log!("Finage: ${:.2}", price);
    SourcePrice { n: "Finage".into(), p: (price * 1_000_000.0).round() as u64, ok: true }
}

fn fetch_pyth(feed_id: &str) -> SourcePrice {
    if feed_id.is_empty() {
        return SourcePrice { n: "Pyth".into(), p: 0, ok: false };
    }
    let clean = feed_id.trim_start_matches("0x");
    let url = format!("{}?ids[]=0x{}&parsed=true", PYTH_HERMES, clean);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() {
        return SourcePrice { n: "Pyth".into(), p: 0, ok: false };
    }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v,
        Err(_) => return SourcePrice { n: "Pyth".into(), p: 0, ok: false },
    };
    let p = &v["parsed"][0]["price"];
    let raw = match p["price"].as_str().and_then(|s| s.parse::<i64>().ok()) {
        Some(r) => r,
        None => return SourcePrice { n: "Pyth".into(), p: 0, ok: false },
    };
    let expo = match p["expo"].as_i64() {
        Some(e) => e,
        None => return SourcePrice { n: "Pyth".into(), p: 0, ok: false },
    };
    let usd = raw as f64 * 10f64.powi(expo as i32);
    log!("Pyth: ${:.2}", usd);
    SourcePrice { n: "Pyth".into(), p: (usd * 1_000_000.0).round() as u64, ok: true }
}

// ── Entry point ─────────────────────────────────────────────────────

pub fn execution_phase() -> Result<()> {
    let raw_input = String::from_utf8(Process::get_inputs())?;
    let input: OracleInput = serde_json::from_str(raw_input.trim())?;

    log!("Session-aware equity oracle: {} @ ts={}", input.symbol, input.timestamp);

    // 1. Determine session state deterministically
    let (session, secs_since_open) = determine_session(input.timestamp, &input.config);
    log!("Session: {} ({}s since open)", session.as_str(), secs_since_open);

    // 2. Fetch from approved sources
    let sources = vec![
        fetch_dxfeed(&input.symbol),
        fetch_finage(&input.symbol),
        fetch_pyth(&input.config.pyth_feed_id),
    ];

    let valid_count = sources.iter().filter(|s| s.ok).count();
    log!("{}/{} sources returned valid prices", valid_count, sources.len());

    // We allow zero valid sources (off-hours) — tally will use EMA from reference
    let result = ExecutionResult {
        sy: input.symbol,
        src: sources,
        ss: session.as_str().to_string(),
        ts: input.timestamp,
        ep: input.config.ema_period,
        vt: input.config.variance_threshold_bps,
        tw: input.config.transition_secs,
        rp: input.config.last_reference_price,
        so: secs_since_open,
    };

    let json_bytes = serde_json::to_vec(&result)?;
    log!("Result: {} bytes", json_bytes.len());
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
