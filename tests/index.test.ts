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

// Monday 2pm ET = 18:00 UTC → in session (NYSE open 13:30–20:00 UTC)
const TS_OPEN = 1751310000; // a Monday ~18:00 UTC
// Saturday 2pm UTC → weekend
const TS_WEEKEND = 1751020800;
// Monday 13:35 UTC → 5 min after open = transition window
const TS_TRANSITION = 1751294100;

function mockSources(dxfeed = DXFEED_NVDA, finage = FINAGE_NVDA, pyth = PYTH_NVDA) {
  fetchMock.mockImplementation((...args: any[]) => {
    const u = String(args[0] || "");
    if (u.includes("dxfeed")) return new Response(JSON.stringify(dxfeed));
    if (u.includes("finage")) return new Response(JSON.stringify(finage));
    if (u.includes("hermes.pyth")) return new Response(JSON.stringify(pyth));
    return new Response(JSON.stringify(dxfeed));
  });
}

function makeInput(timestamp: number, overrides: Record<string, any> = {}) {
  return JSON.stringify({
    symbol: "NVDA",
    timestamp,
    config: {
      open_hour: 13, open_min: 30,
      close_hour: 20, close_min: 0,
      trading_days: [1, 2, 3, 4, 5],
      holidays: [],
      ema_period: 20,
      variance_threshold_bps: 100,
      transition_secs: 900,
      last_reference_price: 135_000_000, // $135 in micro-cents
      pyth_feed_id: "0xtest",
      ...overrides,
    },
  });
}

// ═══════════════════════════════════════════════════════════════════
// REGIME 1: In-session — mark tracks live median
// ═══════════════════════════════════════════════════════════════════

describe("Regime 1: In-session (OPEN)", () => {
  it("execution should detect OPEN session and fetch 3 sources", async () => {
    mockSources();
    const wasm = await file(WASM_PATH).arrayBuffer();
    const vm = await testOracleProgramExecution(
      Buffer.from(wasm), Buffer.from(makeInput(TS_OPEN)), fetchMock
    );
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("OPEN");
    expect(r.src.filter((s: any) => s.ok).length).toBeGreaterThanOrEqual(2);

    console.log("OPEN execution:", JSON.stringify(r, null, 2));
  });

  it("tally should produce median mark ~$135 with high confidence", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();
    const reveal = JSON.stringify({
      sy: "NVDA", ss: "OPEN", ts: TS_OPEN,
      ep: 20, vt: 100, tw: 900, rp: 135_000_000, so: 5400,
      src: [
        { n: "dxFeed", p: 135_000_000, ok: true },
        { n: "Finage", p: 135_000_000, ok: true },
        { n: "Pyth", p: 135_000_000, ok: true },
      ],
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("OPEN");
    expect(r.valid).toBe(true);
    expect(r.conf).toBeGreaterThanOrEqual(90);
    expect(r.mark).toBeCloseTo(135_000_000, -3);
    expect(r.su).toBe(3);

    console.log("OPEN tally:", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// REGIME 2: Weekend — EMA stays stable despite volatile off-hours signal
// ═══════════════════════════════════════════════════════════════════

describe("Regime 2: Weekend (CLOSED_WEEKEND) — manipulation resistance", () => {
  it("tally should barely move mark despite 5% off-hours spike", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    // Off-hours signal spikes to $142 (5% above $135 ref) — manipulation attempt
    const reveal = JSON.stringify({
      sy: "NVDA", ss: "CLOSED_WEEKEND", ts: TS_WEEKEND,
      ep: 20, vt: 100, tw: 900, rp: 135_000_000, so: 0,
      src: [
        { n: "dxFeed", p: 0, ok: false },      // closed
        { n: "Finage", p: 0, ok: false },       // closed
        { n: "Pyth", p: 142_000_000, ok: true }, // thin off-hours signal, spiked
      ],
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("CLOSED_WEEKEND");
    // EMA with period 20: alpha = 2/21 ≈ 0.095
    // EMA = 0.095 * 142 + 0.905 * 135 = 13.49 + 122.18 = 135.67
    // The $7 spike barely moves the mark by ~$0.67
    expect(r.mark).toBeGreaterThan(135_000_000);
    expect(r.mark).toBeLessThan(136_000_000); // stays within $1 of reference
    expect(r.valid).toBe(true);
    expect(r.conf).toBeLessThan(80); // lower confidence off-hours

    console.log("WEEKEND tally (manipulation resistance):", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// REGIME 3: Monday open with 15% gap — smooth transition
// ═══════════════════════════════════════════════════════════════════

describe("Regime 3: Monday open with gap — transition window", () => {
  it("tally should smoothly walk mark from $135 toward $155 over transition", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    // 5 minutes into transition window (300s / 900s = 33% progress)
    // Live price is $155 (15% gap up overnight)
    // Reference was $135
    const reveal = JSON.stringify({
      sy: "NVDA", ss: "TRANSITION", ts: TS_TRANSITION,
      ep: 20, vt: 2000, tw: 900, rp: 135_000_000, so: 300,
      src: [
        { n: "dxFeed", p: 155_000_000, ok: true },
        { n: "Finage", p: 155_200_000, ok: true },
        { n: "Pyth", p: 154_800_000, ok: true },
      ],
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.ss).toBe("TRANSITION");
    // At 33% progress with cubic ease: t ≈ 4 * 0.33^3 ≈ 0.14
    // Mark ≈ 135 * 0.86 + 155 * 0.14 ≈ 137.8
    // NOT a single-block jump to $155
    expect(r.mark).toBeGreaterThan(135_000_000); // moved from reference
    expect(r.mark).toBeLessThan(150_000_000);    // hasn't reached live yet
    expect(r.valid).toBe(true);

    console.log("TRANSITION tally (33% through window):", JSON.stringify(r, null, 2));
  });

  it("at end of transition window, mark should be near live price", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    // 850s into 900s transition window = ~94% progress
    const reveal = JSON.stringify({
      sy: "NVDA", ss: "TRANSITION", ts: TS_TRANSITION + 550,
      ep: 20, vt: 2000, tw: 900, rp: 135_000_000, so: 850,
      src: [
        { n: "dxFeed", p: 155_000_000, ok: true },
        { n: "Finage", p: 155_000_000, ok: true },
        { n: "Pyth", p: 155_000_000, ok: true },
      ],
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    // At 94% progress, cubic ease → t ≈ 0.99
    // Mark should be very close to $155
    expect(r.mark).toBeGreaterThan(153_000_000);

    console.log("TRANSITION tally (94% through window):", JSON.stringify(r, null, 2));
  });
});

// ═══════════════════════════════════════════════════════════════════
// Variance guard — DEFER when mark deviates too far
// ═══════════════════════════════════════════════════════════════════

describe("Variance guard", () => {
  it("should DEFER when open mark deviates >1% from reference", async () => {
    const wasm = await file(WASM_PATH).arrayBuffer();

    // Reference $135, but all sources report $140 = 3.7% deviation > 1% threshold
    const reveal = JSON.stringify({
      sy: "NVDA", ss: "OPEN", ts: TS_OPEN,
      ep: 20, vt: 100, tw: 900, rp: 135_000_000, so: 5400,
      src: [
        { n: "dxFeed", p: 140_000_000, ok: true },
        { n: "Finage", p: 140_000_000, ok: true },
        { n: "Pyth", p: 140_000_000, ok: true },
      ],
    });

    const vm = await testOracleProgramTally(Buffer.from(wasm), Buffer.from("t"),
      [{ exitCode: 0, gasUsed: 0, inConsensus: true, result: Buffer.from(reveal) }]);
    expect(vm.exitCode).toBe(0);
    const r = JSON.parse(Buffer.from(vm.result).toString("utf-8"));

    expect(r.valid).toBe(false); // DEFER
    expect(r.mark).toBe(140_000_000); // still reports the price

    console.log("DEFER result:", JSON.stringify(r, null, 2));
  });
});
