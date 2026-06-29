# SEDA Session-Aware Equity Collateral Oracle

Continuous 24/7 **realizable** collateral value for tokenized equities — not just a reference price, but what a liquidator can actually sell for right now. Triangulates three price signals (underlying, secondary market, NAV), applies redemption-policy awareness, depth discount, and session-state logic.

**Testnet Oracle Program ID:** `20f52da7a3d6514ce2f6b76cb9138e607625e01237b04fe108730821047f0107`

## The Problem

A tokenized stock (e.g. Ondo's tokenized NVDA) has three prices that diverge in stress:

1. **Underlying** — the real equity price. Trades ~6.5 hours/day, 5 days/week.
2. **Token secondary market** — what the token trades for onchain. Trades 24/7 but thin off-hours.
3. **NAV / redemption** — the issuer's mint/redeem price. Looks like the "right" answer, but redemption is typically KYC-gated, which means an anonymous liquidator on a permissionless lending market **cannot access it**.

If a lending protocol uses NAV as collateral value when redemption is inaccessible, and the secondary market depegs, the protocol accumulates bad debt — the price it reports is higher than what any liquidator can actually recover.

## How This Oracle Solves It

### Three-signal triangulation

The oracle carries all three price signals through and doesn't collapse them early:

| Signal | Source | When available |
|--------|--------|----------------|
| Underlying reference | dxFeed, Finage | In-session only |
| Tokenized secondary | Onchain venue prices (config input) | 24/7 |
| NAV / redemption | Issuer data (config input) | When reported |

### Anchor mode — NAV vs secondary

The oracle determines whether NAV is trustworthy as collateral value based on redemption policy:

| Condition | Anchor mode | Mark computation |
|-----------|-------------|-----------------|
| Redemption open + instant + accessible to liquidator + swapper liquid | **NAV_ANCHORED** | Mark near NAV (peg is enforceable) |
| Any condition fails | **SECONDARY_REALIZABLE** | Mark = conservative estimate from secondary market |

### Off-hours composite (not EMA)

Off-hours the underlying is dark. The mark is a reference-anchored **weighted composite** of still-moving signals (secondary venues + crypto proxy), where the reference gets weight = period and each live signal gets weight = 1. Higher period = more anchored to reference = more manipulation-resistant. Stateless — same inputs always produce the same output (required for consensus).

### Depth / liquidity awareness

The mark is capped by what the secondary book can absorb. Thin depth → discounted mark + widened confidence. A top-of-book price on a $15K book is not the same collateral value as a $500K book.

### Session-state logic

- **OPEN:** Multi-source median with MAD outlier rejection
- **CLOSED / PRE / POST:** Reference-anchored composite from secondary + crypto proxy
- **TRANSITION:** Cubic-eased interpolation from off-hours composite to live, over configurable window (default 15 min)
- **Variance guard:** DEFER if mark deviates beyond threshold from reference

## Sample Outputs

### Regime 1: In-session — SECONDARY_REALIZABLE

Redemption inaccessible to liquidators (default). Mark = min(underlying, secondary) = conservative realizable value.

```json
{
  "mark": 134850000, "mf": "134.85",
  "anchor": "SECONDARY_REALIZABLE", "ss": "OPEN",
  "valid": true, "conf": 95, "ra": false,
  "ul": 135000000, "sec": 134850000, "nav": 135000000,
  "dd": 0, "su": 5
}
```

### Regime 2: Weekend — manipulation resistance

Off-hours signals spike to $142 (5% above $135 reference). Composite with period=20 barely moves the pre-discount mark (~$135.62). Depth discount applied on thin $15K book (425bps).

```json
{
  "mark": 129852233, "mf": "129.85",
  "anchor": "SECONDARY_REALIZABLE", "ss": "CLOSED_WEEKEND",
  "valid": true, "conf": 43, "ra": false,
  "ul": 0, "sec": 141750000, "nav": 135000000,
  "dd": 425, "su": 3
}
```

### Regime 3: Monday open with 15% gap — smooth transition

Live sources report $155. Mark walks smoothly from off-hours composite toward live level. No single-block jump.

```json
{
  "mark": 139509175, "mf": "139.51",
  "anchor": "SECONDARY_REALIZABLE", "ss": "TRANSITION",
  "valid": true, "conf": 72, "ra": false,
  "ul": 155100000, "sec": 154650000, "nav": 135000000,
  "dd": 0, "su": 5
}
```

### Regime 4: Depeg with inaccessible redemption — the bad-debt proof

Token depegs: NAV = $135 but secondary trades at $120. Redemption is open but KYC-gated — a permissionless liquidator **cannot redeem at NAV**. The oracle reports the secondary realizable value ($116.76 after depth discount), not NAV.

```json
{
  "mark": 116756250, "mf": "116.76",
  "anchor": "SECONDARY_REALIZABLE", "ss": "OPEN",
  "valid": true, "conf": 85, "ra": false,
  "ul": 135000000, "sec": 119750000, "nav": 135000000,
  "dd": 250, "su": 5
}
```

**This is the critical case.** A naive oracle reporting NAV ($135) would tell the lending protocol the collateral is fine. This oracle reports $116.76 — what a liquidator can actually recover. That $18.24 difference is the bad debt the protocol avoids.

### Regime 5: Redemption accessible — NAV_ANCHORED

Redemption open + instant + accessible to liquidators + swapper liquid. Secondary dips to $130 on thin volume, but the peg is enforceable — mark anchors at NAV.

```json
{
  "mark": 135000000, "mf": "135.00",
  "anchor": "NAV_ANCHORED", "ss": "OPEN",
  "valid": true, "conf": 95, "ra": true,
  "ul": 135000000, "sec": 130000000, "nav": 135000000,
  "dd": 0, "su": 4
}
```

### Variance guard — DEFER

Mark deviates >1% from reference. Oracle flags `valid: false` rather than emitting suspect data.

```json
{
  "mark": 139000000, "mf": "139.00",
  "anchor": "SECONDARY_REALIZABLE", "ss": "OPEN",
  "valid": false, "conf": 85, "ra": false, "dd": 0
}
```

## Input Parameters

```json
{
  "symbol": "NVDA",
  "timestamp": 1751310000,
  "config": {
    "open_hour": 13, "open_min": 30,
    "close_hour": 20, "close_min": 0,
    "trading_days": [1, 2, 3, 4, 5],
    "holidays": ["2026-07-04"],
    "ema_period": 20,
    "variance_threshold_bps": 100,
    "transition_secs": 900,
    "last_reference_price": 135000000,
    "pyth_feed_id": "0x..."
  },
  "redemption": {
    "open": true,
    "instant": true,
    "accessible_to_liquidator": false,
    "redemption_asset": "USDC",
    "swapper_liquidity_sufficient": true,
    "nav_price": 135000000
  },
  "secondary": {
    "venue_prices": [134800000, 134900000],
    "venue_names": ["Uniswap", "Curve"],
    "depth_usd": 200000,
    "depth_threshold_usd": 100000
  }
}
```

| Section | Key fields |
|---------|------------|
| **config** | Session calendar, anchor period (higher = more manipulation-resistant), variance guard, transition window |
| **redemption** | Policy state: `accessible_to_liquidator` governs NAV_ANCHORED vs SECONDARY_REALIZABLE |
| **secondary** | Onchain venue prices + order book depth for realizable value computation |

No secrets in inputs — all data sources use public endpoints.

## Output Fields

| Field | Description |
|-------|-------------|
| `mark` | **Realizable collateral value** (micro-cents) |
| `anchor` | `NAV_ANCHORED` or `SECONDARY_REALIZABLE` |
| `valid` | `true` = safe to use, `false` = DEFER |
| `conf` | Confidence 0–100 |
| `ra` | Redemption accessible to liquidator? |
| `ul` / `sec` / `nav` | The three raw signal prices |
| `dd` | Depth discount applied (basis points) |

## Build & Test

```bash
bun install
make build
bun test tests/    # 6 tests: in-session, weekend manipulation, transition gap,
                   # depeg-inaccessible (bad-debt proof), NAV-anchored, variance guard
```

## Architecture

```
src/
  main.rs              — SEDA oracle program entry point
  execution_phase.rs   — Three-signal fetch + session detection + redemption state
  tally_phase.rs       — Triangulation, anchor mode, composite, depth discount, guards
tests/
  index.test.ts        — 6 tests across all regimes including depeg proof
```

Built on [SEDA](https://seda.xyz) — custom oracle logic, any data, one endpoint.
