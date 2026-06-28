import { describe, it, expect } from "vitest";
import { humanBytes, humanRate, etaHuman, percent, bytesToGiB, giBToBytes } from "./format";

describe("humanBytes", () => {
  it("formats zero and sub-KiB without decimals", () => {
    expect(humanBytes(0)).toBe("0 B");
    expect(humanBytes(512)).toBe("512 B");
    expect(humanBytes(1023)).toBe("1023 B");
  });

  it("scales to KiB/MiB/GiB with one decimal", () => {
    expect(humanBytes(1024)).toBe("1.0 KiB");
    expect(humanBytes(1536)).toBe("1.5 KiB");
    expect(humanBytes(1024 * 1024)).toBe("1.0 MiB");
    expect(humanBytes(3 * 1024 * 1024 * 1024)).toBe("3.0 GiB");
  });

  it("clamps negatives to 0 B", () => {
    expect(humanBytes(-1)).toBe("0 B");
    expect(humanBytes(-1024)).toBe("0 B");
  });
});

describe("humanRate", () => {
  it("renders non-positive/non-finite rates as an em dash", () => {
    expect(humanRate(0)).toBe("—");
    expect(humanRate(-1)).toBe("—");
    expect(humanRate(Number.NaN)).toBe("—");
    expect(humanRate(Number.POSITIVE_INFINITY)).toBe("—");
  });

  it("formats a byte rate with a /s suffix, scaling like humanBytes", () => {
    expect(humanRate(512)).toBe("512 B/s");
    expect(humanRate(1024)).toBe("1.0 KiB/s");
    expect(humanRate(1.5 * 1024 * 1024)).toBe("1.5 MiB/s");
    expect(humanRate(2 * 1024 * 1024 * 1024)).toBe("2.0 GiB/s");
  });
});

describe("etaHuman", () => {
  it("renders null/undefined/negative as an em dash", () => {
    expect(etaHuman(null)).toBe("—");
    expect(etaHuman(undefined)).toBe("—");
    expect(etaHuman(-5)).toBe("—");
  });

  it("formats sub-hour values as M:SS with zero-padded seconds", () => {
    expect(etaHuman(0)).toBe("0:00");
    expect(etaHuman(5)).toBe("0:05");
    expect(etaHuman(90)).toBe("1:30");
    expect(etaHuman(605)).toBe("10:05");
  });

  it("rolls over into H:MM:SS past an hour", () => {
    expect(etaHuman(3600)).toBe("1:00:00");
    expect(etaHuman(3661)).toBe("1:01:01");
  });
});

describe("percent", () => {
  it("returns 0 when total <= 0 (no divide-by-zero)", () => {
    expect(percent(0, 0)).toBe(0);
    expect(percent(10, 0)).toBe(0);
    expect(percent(10, -5)).toBe(0);
  });

  it("computes a rounded integer percentage", () => {
    expect(percent(1, 4)).toBe(25);
    expect(percent(1, 3)).toBe(33);
    expect(percent(2, 3)).toBe(67);
  });

  it("clamps to the 0..100 range", () => {
    expect(percent(-5, 100)).toBe(0);
    expect(percent(150, 100)).toBe(100);
  });

  it("returns 0 for non-finite done (no NaN/Infinity leaking into width/aria)", () => {
    expect(percent(Number.NaN, 100)).toBe(0);
    expect(percent(Number.POSITIVE_INFINITY, 100)).toBe(0);
    expect(percent(Number.NEGATIVE_INFINITY, 100)).toBe(0);
  });
});

describe("bytesToGiB / giBToBytes", () => {
  it("converts using 1024^3", () => {
    expect(bytesToGiB(1024 * 1024 * 1024)).toBe(1);
    expect(bytesToGiB(3 * 1024 * 1024 * 1024)).toBe(3);
    expect(giBToBytes(1)).toBe(1024 * 1024 * 1024);
    expect(giBToBytes(2)).toBe(2 * 1024 * 1024 * 1024);
  });

  it("round-trips a whole-GiB value", () => {
    expect(bytesToGiB(giBToBytes(5))).toBe(5);
  });

  it("clamps negative GiB to 0", () => {
    expect(giBToBytes(-1)).toBe(0);
  });
});
