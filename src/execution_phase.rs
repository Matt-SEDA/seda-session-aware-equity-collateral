use anyhow::Result;
use seda_sdk_rs::{http_fetch, log, Process};
use serde::{Deserialize, Serialize};

/// Session-Aware Equity Collateral Oracle — Execution Phase (v2)
///
/// Triangulates three price signals, carries all through to tally:
///   1. Underlying reference — real equity price (dxFeed, Finage). Live only in-session.
///   2. Tokenized secondary — what the token actually trades for onchain.
///   3. NAV / redemption value — the issuer's mint/redeem price.
///
/// Also carries redemption policy state and depth/liquidity config so the
/// tally phase can compute realizable value (what a liquidator can sell for),
/// not just a reference mark.

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
    pub symbol: String,
    pub timestamp: u64,
    #[serde(default)]
    pub config: SessionConfig,
    #[serde(default)]
    pub redemption: RedemptionState,
    #[serde(default)]
    pub secondary: SecondaryConfig,
}

#[derive(Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_open_hour")]
    pub open_hour: u32,
    #[serde(default = "default_open_min")]
    pub open_min: u32,
    #[serde(default = "default_close_hour")]
    pub close_hour: u32,
    #[serde(default = "default_close_min")]
    pub close_min: u32,
    #[serde(default = "default_trading_days")]
    pub trading_days: Vec<u32>,
    #[serde(default)]
    pub holidays: Vec<String>,
    #[serde(default = "default_ema_period")]
    pub ema_period: u32,
    #[serde(default = "default_variance_bps")]
    pub variance_threshold_bps: u64,
    #[serde(default = "default_transition_secs")]
    pub transition_secs: u64,
    #[serde(default)]
    pub last_reference_price: u64,
    #[serde(default)]
    pub pyth_feed_id: String,
}

fn default_open_hour() -> u32 { 13 }
fn default_open_min() -> u32 { 30 }
fn default_close_hour() -> u32 { 20 }
fn default_close_min() -> u32 { 0 }
fn default_trading_days() -> Vec<u32> { vec![1, 2, 3, 4, 5] }
fn default_ema_period() -> u32 { 20 }
fn default_variance_bps() -> u64 { 100 }
fn default_transition_secs() -> u64 { 900 }

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            open_hour: 13, open_min: 30, close_hour: 20, close_min: 0,
            trading_days: default_trading_days(), holidays: Vec::new(),
            ema_period: 20, variance_threshold_bps: 100, transition_secs: 900,
            last_reference_price: 0, pyth_feed_id: String::new(),
        }
    }
}

/// Redemption policy state — governs whether NAV is realizable
#[derive(Deserialize, Serialize, Clone)]
pub struct RedemptionState {
    /// Is the redemption window currently open?
    #[serde(default)]
    pub open: bool,
    /// Is redemption instant (vs T+1, T+2, etc.)?
    #[serde(default)]
    pub instant: bool,
    /// Can an anonymous liquidator on a permissionless market actually redeem?
    /// Default false — most tokenized equities require KYC.
    #[serde(default)]
    pub accessible_to_liquidator: bool,
    /// Redemption asset (e.g. "USDC", "USDon")
    #[serde(default)]
    pub redemption_asset: String,
    /// Is the stablecoin swapper liquid enough for the redemption?
    #[serde(default)]
    pub swapper_liquidity_sufficient: bool,
    /// Issuer-reported NAV price (micro-cents). 0 = unknown.
    #[serde(default)]
    pub nav_price: u64,
}

impl Default for RedemptionState {
    fn default() -> Self {
        Self {
            open: false, instant: false, accessible_to_liquidator: false,
            redemption_asset: String::new(), swapper_liquidity_sufficient: false,
            nav_price: 0,
        }
    }
}

/// Secondary market config
#[derive(Deserialize, Serialize, Clone)]
pub struct SecondaryConfig {
    /// Secondary venue prices (micro-cents). Multiple venues for median.
    #[serde(default)]
    pub venue_prices: Vec<u64>,
    /// Venue names corresponding to venue_prices
    #[serde(default)]
    pub venue_names: Vec<String>,
    /// Estimated depth at top-of-book (USD notional the book can absorb)
    #[serde(default)]
    pub depth_usd: u64,
    /// Depth threshold — below this, apply discount (default $100K)
    #[serde(default = "default_depth_threshold")]
    pub depth_threshold_usd: u64,
}

fn default_depth_threshold() -> u64 { 100_000 }

impl Default for SecondaryConfig {
    fn default() -> Self {
        Self {
            venue_prices: Vec::new(), venue_names: Vec::new(),
            depth_usd: 0, depth_threshold_usd: default_depth_threshold(),
        }
    }
}

// ── Source price ─────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct SourcePrice {
    pub n: String,
    pub p: u64,
    pub ok: bool,
}

// ── Execution result ────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct ExecutionResult {
    pub sy: String,
    /// Signal 1: Underlying reference prices (dxFeed, Finage)
    pub underlying: Vec<SourcePrice>,
    /// Signal 2: Tokenized secondary venue prices
    pub secondary: Vec<SourcePrice>,
    /// Signal 3: NAV / redemption value (micro-cents, 0 if unknown)
    pub nav: u64,
    /// Pyth price (crypto proxy / still-moving signal for off-hours)
    pub pyth: SourcePrice,
    /// Session state
    pub ss: String,
    pub ts: u64,
    pub ep: u32,
    pub vt: u64,
    pub tw: u64,
    pub rp: u64,
    pub so: u64,
    /// Redemption state (passed through for tally)
    pub rd: RedemptionState,
    /// Depth USD
    pub dp: u64,
    /// Depth threshold USD
    pub dt: u64,
}

// ── Session detection ───────────────────────────────────────────────

fn determine_session(ts: u64, config: &SessionConfig) -> (SessionState, u64) {
    let days_since_epoch = ts / 86400;
    let day_of_week = ((days_since_epoch + 4) % 7) as u32;
    let seconds_in_day = ts % 86400;
    let hour = (seconds_in_day / 3600) as u32;
    let minute = ((seconds_in_day % 3600) / 60) as u32;
    let time_minutes = hour * 60 + minute;
    let open_minutes = config.open_hour * 60 + config.open_min;
    let close_minutes = config.close_hour * 60 + config.close_min;

    let is_trading_day = config.trading_days.contains(&day_of_week);
    let date_str = unix_to_date_str(ts);
    let is_holiday = config.holidays.iter().any(|h| h == &date_str);

    if !is_trading_day { return (SessionState::ClosedWeekend, 0); }
    if is_holiday { return (SessionState::ClosedHoliday, 0); }
    if time_minutes < open_minutes { return (SessionState::Pre, 0); }
    if time_minutes >= close_minutes { return (SessionState::Post, 0); }

    let secs_since_open = (time_minutes - open_minutes) as u64 * 60 + (seconds_in_day % 60);
    if secs_since_open < config.transition_secs {
        return (SessionState::Transition, secs_since_open);
    }
    (SessionState::Open, secs_since_open)
}

fn unix_to_date_str(ts: u64) -> String {
    let days = (ts / 86400) as i64;
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
    let url = format!("{}/events.json?events=Quote&symbols={}", DXFEED_API, symbol);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return SourcePrice { n: "dxFeed".into(), p: 0, ok: false }; }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v, Err(_) => return SourcePrice { n: "dxFeed".into(), p: 0, ok: false },
    };
    let quote = &v["Quote"];
    let bid = quote[symbol]["bidPrice"].as_f64().or_else(|| quote[symbol]["lastPrice"].as_f64());
    let ask = quote[symbol]["askPrice"].as_f64();
    let price = match (bid, ask) {
        (Some(b), Some(a)) if b > 0.0 && a > 0.0 => (b + a) / 2.0,
        (Some(b), _) if b > 0.0 => b,
        (_, Some(a)) if a > 0.0 => a,
        _ => {
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
            return SourcePrice { n: "dxFeed".into(), p: 0, ok: false };
        }
    };
    log!("dxFeed: ${:.2}", price);
    SourcePrice { n: "dxFeed".into(), p: (price * 1_000_000.0).round() as u64, ok: true }
}

fn fetch_finage(symbol: &str) -> SourcePrice {
    let url = format!("{}/{}?apikey=demo", FINAGE_API, symbol);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return SourcePrice { n: "Finage".into(), p: 0, ok: false }; }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v, Err(_) => return SourcePrice { n: "Finage".into(), p: 0, ok: false },
    };
    let bid = v["bid"].as_f64().unwrap_or(0.0);
    let ask = v["ask"].as_f64().unwrap_or(0.0);
    let last = v["last"].as_f64().or_else(|| v["price"].as_f64()).unwrap_or(0.0);
    let price = if bid > 0.0 && ask > 0.0 { (bid + ask) / 2.0 } else if last > 0.0 { last } else {
        return SourcePrice { n: "Finage".into(), p: 0, ok: false };
    };
    log!("Finage: ${:.2}", price);
    SourcePrice { n: "Finage".into(), p: (price * 1_000_000.0).round() as u64, ok: true }
}

fn fetch_pyth(feed_id: &str) -> SourcePrice {
    if feed_id.is_empty() { return SourcePrice { n: "Pyth".into(), p: 0, ok: false }; }
    let clean = feed_id.trim_start_matches("0x");
    let url = format!("{}?ids[]=0x{}&parsed=true", PYTH_HERMES, clean);
    let resp = http_fetch(&url, None);
    if !resp.is_ok() { return SourcePrice { n: "Pyth".into(), p: 0, ok: false }; }
    let v: serde_json::Value = match serde_json::from_slice(&resp.bytes) {
        Ok(v) => v, Err(_) => return SourcePrice { n: "Pyth".into(), p: 0, ok: false },
    };
    let p = &v["parsed"][0]["price"];
    let raw = match p["price"].as_str().and_then(|s| s.parse::<i64>().ok()) {
        Some(r) => r, None => return SourcePrice { n: "Pyth".into(), p: 0, ok: false },
    };
    let expo = match p["expo"].as_i64() {
        Some(e) => e, None => return SourcePrice { n: "Pyth".into(), p: 0, ok: false },
    };
    let usd = raw as f64 * 10f64.powi(expo as i32);
    log!("Pyth: ${:.2}", usd);
    SourcePrice { n: "Pyth".into(), p: (usd * 1_000_000.0).round() as u64, ok: true }
}

// ── Entry point ─────────────────────────────────────────────────────

pub fn execution_phase() -> Result<()> {
    let raw_input = String::from_utf8(Process::get_inputs())?;
    let input: OracleInput = serde_json::from_str(raw_input.trim())?;

    log!("Equity collateral oracle v2: {} @ ts={}", input.symbol, input.timestamp);

    let (session, secs_since_open) = determine_session(input.timestamp, &input.config);
    log!("Session: {}", session.as_str());

    // Signal 1: Underlying reference (live equity sources)
    let underlying = vec![
        fetch_dxfeed(&input.symbol),
        fetch_finage(&input.symbol),
    ];
    let ul_valid = underlying.iter().filter(|s| s.ok).count();
    log!("Underlying: {}/{} live", ul_valid, underlying.len());

    // Signal 2: Secondary venue prices (passed in config — onchain data)
    let secondary: Vec<SourcePrice> = input.secondary.venue_prices.iter()
        .enumerate()
        .map(|(i, &p)| {
            let name = input.secondary.venue_names.get(i)
                .cloned()
                .unwrap_or_else(|| format!("Venue{}", i + 1));
            SourcePrice { n: name, p, ok: p > 0 }
        })
        .collect();
    let sec_valid = secondary.iter().filter(|s| s.ok).count();
    log!("Secondary venues: {}/{}", sec_valid, secondary.len());

    // Signal 3: NAV / redemption value
    let nav = input.redemption.nav_price;
    if nav > 0 {
        log!("NAV: ${:.2}", nav as f64 / 1_000_000.0);
    }

    // Crypto proxy (Pyth — still moves off-hours)
    let pyth = fetch_pyth(&input.config.pyth_feed_id);

    let result = ExecutionResult {
        sy: input.symbol,
        underlying,
        secondary,
        nav,
        pyth,
        ss: session.as_str().to_string(),
        ts: input.timestamp,
        ep: input.config.ema_period,
        vt: input.config.variance_threshold_bps,
        tw: input.config.transition_secs,
        rp: input.config.last_reference_price,
        so: secs_since_open,
        rd: input.redemption,
        dp: input.secondary.depth_usd,
        dt: input.secondary.depth_threshold_usd,
    };

    let json_bytes = serde_json::to_vec(&result)?;
    log!("Result: {} bytes", json_bytes.len());
    Process::success(&json_bytes);

    #[allow(unreachable_code)]
    Ok(())
}
