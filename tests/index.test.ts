import { afterEach, describe, it, expect, mock } from "bun:test";
import { file } from "bun";
import { testOracleProgramExecution, testOracleProgramTally } from "@seda-protocol/dev-tools";

const WASM_PATH = "target/wasm32-wasip1/release-wasm/session-aware-equity-collateral.wasm";
const fetchMock = mock();
afterEach(() => { fetchMock.mockRestore(); });

// NVDA ~$135
const DXFEED_NVDA = { Quote: { NVDA: { bidPrice: 134.80, askPrice: 135.20 } } };
const FINAGE_NVDA = { symbol: "NVDA", bid: 134.90, ask: 135.10 };
const PYTH_NVDA = { parsed: [{ price: { price: "13500000000", conf: "5000000", expo: -8, publish_time: 1750000000 } }] };

const TS_OPEN = 1751310000;       // Monday ~18:00 UTC (in session)
const TS_WEEKEND = 1751020800;    // Saturday
const TS_TRANSITION = 1751294100; // Monday 13:35 UTC (5min after open)

function mockSources() {
  fetchMock.mockImplementation((...args: any[]) => {
    const u = String(args[0] || "");
    if (u.includes("dxfeed")) return new Response(JSON.stringify(DXFEED_NVDA));
    if (u.includes("finage")) return new Response(JSON.stringify(FINAGE_NVDA));
    if (u.includes("hermes.pyth")) return new Response(JSON.stringify(PYTH_NVDA));
    return new Response(JSON.stringify(DXFEED_NVDA));
  });
}

// Helper: build a tally reveal with three signals + redemption state
function makeReveal(overrides: Record<string, any> = {}) {
  return JSON.stringify({
    sy: "NVDA",
    underlying: [
      { n: "dxFeed", p: 135_000_000, ok: true },
      { n: "Finage", p: 135_000_000, ok: true },
    ],
    secondary: [
      { n: "Uniswap", p: 134_800_000, ok: true },
      { n: "Curve", p: 134_900_000, ok: true },
    ],
    nav: 135_000_000,
    pyth: { n: "Pyth", p: 135_000_000, ok: true },
    ss: "OPEN", ts: TS_OPEN,
    ep: 20, vt: 100, tw: 900, rp: 135_000_000, so: 5400,
    rd: {
      open: false, instant: false, accessible_to_liquidator: false,
      redemption_asset: "USDC", swapper_liquidity_sufficient: false,
      nav_price: 135_000_000,
    },
    dp: 200_000, dt: 100_000,
    ...overrides,
  });
}

// ═══════════════════════════════════════════════════════════════════
// REGIME 1: In-session — mark tracks live underlying
// ═══════════════════════════════════════════════════════════════════

describe("Regime 1: In-session (OPEN)", () => {
  it("should produce SECONDARY_REALIZABLE mark from underlying + secondary", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = makeReveal(); // default: redemption not accessible

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("OPEN");
    expect(r.anchor).toBe("SECONDARY_REALIZABLE");
    expect(r.ra).toBe(false); // liquidator can't redeem
    expect(r.valid).toBe(true);
    expect(r.conf).toBeGreaterThanOrEqual(80);
    // Mark should be min(underlying, secondary) ≈ $134.80–$135.00
    expect(r.mark).toBeLessThanOrEqual(135_000_000);
    expect(r.mark).toBeGreaterThan(134_000_000);

    console.log("OPEN SECONDARY_REALIZABLE:", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// REGIME 2: Weekend — EMA resists manipulation
// ═══════════════════════════════════════════════════════════════════

describe("Regime 2: Weekend — manipulation resistance", () => {
  it("should barely move mark despite volatile off-hours signal", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    // Off-hours: underlying dark, secondary + pyth spiked to $142
    const reveal = makeReveal({
      ss: "CLOSED_WEEKEND", so: 0,
      underlying: [
        { n: "dxFeed", p: 0, ok: false },
        { n: "Finage", p: 0, ok: false },
      ],
      secondary: [
        { n: "Uniswap", p: 142_000_000, ok: true },
        { n: "Curve", p: 141_500_000, ok: true },
      ],
      pyth: { n: "Pyth", p: 141_800_000, ok: true },
      dp: 15_000, dt: 100_000, // thin depth
      vt: 500, // 5% variance threshold (depth discount + spike = >1%)
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("CLOSED_WEEKEND");
    // Composite with period 20: reference anchors heavily.
    // Pre-discount composite ≈ $135.62 (spike barely moves it).
    // Depth discount on $15K book (425bps) brings it to ~$129.85.
    // The depth discount is intentional — a thin book IS less realizable.
    expect(r.mark).toBeGreaterThan(128_000_000);
    expect(r.mark).toBeLessThan(136_000_000);
    expect(r.dd).toBeGreaterThan(0); // depth discount applied on thin book
    expect(r.valid).toBe(true);

    console.log("WEEKEND manipulation resistance:", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// REGIME 3: Monday open with 15% gap — smooth transition
// ═══════════════════════════════════════════════════════════════════

describe("Regime 3: Monday open with gap — transition window", () => {
  it("should smoothly walk mark toward live price", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    // 5 min into transition, live at $155 (15% gap up)
    const reveal = makeReveal({
      ss: "TRANSITION", so: 300, vt: 2000, // wide threshold for gap
      underlying: [
        { n: "dxFeed", p: 155_000_000, ok: true },
        { n: "Finage", p: 155_200_000, ok: true },
      ],
      secondary: [
        { n: "Uniswap", p: 154_500_000, ok: true },
        { n: "Curve", p: 154_800_000, ok: true },
      ],
      pyth: { n: "Pyth", p: 154_900_000, ok: true },
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("TRANSITION");
    // Should NOT jump straight to $155 — interpolating smoothly
    expect(r.mark).toBeGreaterThan(135_000_000);
    expect(r.mark).toBeLessThan(150_000_000);
    expect(r.valid).toBe(true);

    console.log("TRANSITION (33% through):", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// REGIME 4: Depeg — redemption inaccessible → SECONDARY_REALIZABLE
// ═══════════════════════════════════════════════════════════════════

describe("Regime 4: Depeg with inaccessible redemption — the bad-debt proof", () => {
  it("should report discounted secondary price, NOT NAV", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    // Token depegs: NAV = $135 but secondary trades at $120 (11% below)
    // Redemption is open but NOT accessible to anonymous liquidators (KYC-gated)
    const reveal = makeReveal({
      ss: "OPEN", so: 5400,
      underlying: [
        { n: "dxFeed", p: 135_000_000, ok: true },
        { n: "Finage", p: 135_000_000, ok: true },
      ],
      secondary: [
        { n: "Uniswap", p: 120_000_000, ok: true },
        { n: "Curve", p: 119_500_000, ok: true },
      ],
      nav: 135_000_000,
      rp: 135_000_000,
      vt: 2000, // wide threshold to allow the depeg through
      rd: {
        open: true, instant: true,
        accessible_to_liquidator: false, // KEY: liquidator can't redeem
        redemption_asset: "USDon",
        swapper_liquidity_sufficient: true,
        nav_price: 135_000_000,
      },
      dp: 50_000, dt: 100_000, // thin secondary depth
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.anchor).toBe("SECONDARY_REALIZABLE"); // NOT NAV_ANCHORED
    expect(r.ra).toBe(false); // redemption not accessible
    // Mark should be around $120 (secondary), NOT $135 (NAV)
    expect(r.mark).toBeLessThan(125_000_000);
    expect(r.nav).toBe(135_000_000); // NAV reported but not used as mark
    expect(r.dd).toBeGreaterThan(0); // depth discount on thin book
    expect(r.valid).toBe(true);

    console.log("DEPEG (inaccessible redemption):", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// REGIME 5: Inverse — redemption accessible → NAV_ANCHORED
// ═══════════════════════════════════════════════════════════════════

describe("Regime 5: Redemption accessible — anchors near NAV", () => {
  it("should anchor near NAV even if thin secondary dips", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    // Secondary dips to $130 on thin volume, but redemption is fully accessible
    const reveal = makeReveal({
      ss: "OPEN", so: 5400,
      underlying: [
        { n: "dxFeed", p: 135_000_000, ok: true },
        { n: "Finage", p: 135_000_000, ok: true },
      ],
      secondary: [
        { n: "Uniswap", p: 130_000_000, ok: true },
      ],
      nav: 135_000_000,
      rp: 135_000_000,
      rd: {
        open: true, instant: true,
        accessible_to_liquidator: true, // liquidator CAN redeem
        redemption_asset: "USDC",
        swapper_liquidity_sufficient: true,
        nav_price: 135_000_000,
      },
      dp: 200_000, dt: 100_000,
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.anchor).toBe("NAV_ANCHORED"); // peg is enforceable
    expect(r.ra).toBe(true);
    // Mark should be near $135 (NAV), NOT dragged down to $130 secondary
    expect(r.mark).toBeGreaterThan(134_000_000);
    expect(r.dd).toBe(0); // no depth discount in NAV_ANCHORED mode

    console.log("NAV_ANCHORED (accessible redemption):", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// Variance guard
// ═══════════════════════════════════════════════════════════════════

describe("Variance guard", () => {
  it("should DEFER when mark exceeds threshold", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = makeReveal({
      underlying: [
        { n: "dxFeed", p: 140_000_000, ok: true },
        { n: "Finage", p: 140_000_000, ok: true },
      ],
      secondary: [
        { n: "Uniswap", p: 139_000_000, ok: true },
      ],
      vt: 100, // 1% threshold, but price moved 3.7%
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.valid).toBe(false);
    console.log("DEFER:", JSON.stringify(r, null, 2));
  });
});
