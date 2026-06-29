# SEDA Session-Aware Equity Collateral Oracle

Continuous 24/7 mark price for tokenized equities used as lending collateral — including weekends and holidays when the underlying market is closed. Multi-source median in-session, self-referential EMA off-hours, smooth transition at reopening.

**Testnet Oracle Program ID:** `c00f2bd15724ca9a518f482ed82ffcf6cba4ebbc3029220adf037baa49960371`

## The Problem

Tokenized equities (NVDA, TSLA, AAPL) trade 24/7 onchain, but the underlying equity market is open ~6.5 hours/day, 5 days/week. Lending protocols using these as collateral need a mark price that:

1. **Never freezes** — a stale price during off-hours means liquidations can't fire when needed
2. **Resists manipulation** — thin off-hours liquidity makes it trivial to spike the price and drain a lending pool
3. **Doesn't gap** — a 15% Monday-morning jump in one block triggers a cascade of simultaneous liquidations

This oracle solves all three.

## How It Works

### Three Regimes

**In-session (OPEN):** Multi-source median (dxFeed, Finage, Pyth) with MAD outlier rejection. One wick or one manipulated venue can't move the mark.

**Off-hours (CLOSED/PRE/POST):** Self-referential EMA seeded from the last in-session mark, updated by whatever signals are still moving (onchain venue price, crypto proxy). With EMA period 20, alpha = 0.095 — a $7 spike on a thin off-hours venue only moves the mark by ~$0.67. Manipulation-resistant by construction.

**Transition (just-opened window):** Cubic-eased interpolation from the off-hours mark to the live composite over a configurable window (default 15 minutes). The mark walks smoothly to the new level instead of jumping in one block — no reopening-gap liquidation cascade.

### Variance Guard

If the computed mark deviates beyond the configured threshold (default 1%) from the last known reference, the oracle flags `valid: false` (DEFER) rather than emitting a suspect price. The lending protocol can hold the previous mark instead of acting on bad data.

## Sample Outputs

### Regime 1: In-session — mark tracks live median

**Request:**
```json
{
  "symbol": "NVDA",
  "timestamp": 1751310000,
  "config": { "last_reference_price": 135000000 }
}
```

**Response:**
```json
{
  "mark": 135000000,
  "mf": "135.00",
  "ss": "OPEN",
  "valid": true,
  "conf": 95,
  "su": 3,
  "sn": ["dxFeed", "Finage", "Pyth"],
  "sy": "NVDA"
}
```

### Regime 2: Weekend — manipulation-resistant continuous mark

**Request:** Same symbol, Saturday timestamp. Off-hours signal spikes to $142 (5% above $135 reference).

**Response:**
```json
{
  "mark": 135666667,
  "mf": "135.67",
  "ss": "CLOSED_WEEKEND",
  "valid": true,
  "conf": 60,
  "su": 1,
  "sn": ["Pyth"],
  "sy": "NVDA"
}
```

The $7 spike moved the mark by only $0.67. A thin off-hours print cannot move the collateral mark materially.

### Regime 3: Monday open with 15% gap — smooth transition

**Request:** Monday, 5 minutes after open. Live sources report $155 (15% gap up from $135 reference).

**Response (5 min in, 33% through window):**
```json
{
  "mark": 137837945,
  "mf": "137.84",
  "ss": "TRANSITION",
  "valid": true,
  "conf": 71,
  "su": 3,
  "sn": ["dxFeed", "Finage", "Pyth"],
  "sy": "NVDA"
}
```

**Response (14 min in, 94% through window):**
```json
{
  "mark": 154809523,
  "mf": "154.81",
  "ss": "TRANSITION",
  "valid": true,
  "conf": 93,
  "su": 3,
  "sn": ["dxFeed", "Finage", "Pyth"],
  "sy": "NVDA"
}
```

No single-block jump from $135 to $155. The mark walks there smoothly over 15 minutes using cubic easing.

## Input Parameters

```json
{
  "symbol": "NVDA",
  "timestamp": 1751310000,
  "config": {
    "open_hour": 13, "open_min": 30,
    "close_hour": 20, "close_min": 0,
    "trading_days": [1, 2, 3, 4, 5],
    "holidays": ["2026-07-04", "2026-12-25"],
    "ema_period": 20,
    "variance_threshold_bps": 100,
    "transition_secs": 900,
    "last_reference_price": 135000000,
    "pyth_feed_id": "0x..."
  }
}
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `symbol` | — | Equity ticker (NVDA, TSLA, AAPL) |
| `timestamp` | — | Reference time (unix secs) — the program's deterministic clock |
| `open_hour/min` | 13:30 UTC | NYSE open |
| `close_hour/min` | 20:00 UTC | NYSE close |
| `trading_days` | Mon–Fri | ISO weekday numbers |
| `holidays` | [] | Dates as "YYYY-MM-DD" |
| `ema_period` | 20 | Off-hours EMA period (higher = more manipulation-resistant) |
| `variance_threshold_bps` | 100 | Max deviation from reference before DEFER (100 = 1%) |
| `transition_secs` | 900 | Reopening interpolation window (900 = 15 min) |
| `last_reference_price` | 0 | Last known in-session mark (micro-cents) for EMA seed |
| `pyth_feed_id` | "" | Optional Pyth Hermes feed ID |

No secrets in inputs — all data sources use public endpoints.

## Data Sources

| Source | Data | Auth |
|--------|------|------|
| **dxFeed** | Equity quotes (bid/ask mid) | Public (15-min delayed on demo) |
| **Finage** | Real-time equity data | Public (demo key) |
| **Pyth Hermes** | Equity/crypto prices | Public (no auth) |

Never Binance, CoinGecko, or CoinMarketCap.

## Build & Test

```bash
bun install
make build
bun test tests/    # 6 tests: open session, weekend manipulation resistance,
                   # transition early/late, variance guard DEFER
```

## Architecture

```
src/
  main.rs              — SEDA oracle program entry point
  execution_phase.rs   — Multi-source fetch + deterministic session detection
  tally_phase.rs       — Median/EMA/transition/variance-guard computation
tests/
  index.test.ts        — 6 tests across all three regimes
```

Built on [SEDA](https://seda.xyz) — custom oracle logic, any data, one endpoint.
