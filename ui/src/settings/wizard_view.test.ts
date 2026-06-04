import { describe, it, expect } from "vitest";
import { defaultConfigView, withCloud } from "./wizard_view";
import type { CloudView } from "../lib/bindings";

describe("defaultConfigView", () => {
  it("mirrors the Rust M1 defaults with the given destination", () => {
    const v = defaultConfigView("/Users/me/GPBeam");
    expect(v.destRoot).toBe("/Users/me/GPBeam");
    expect(v.filenameTemplate).toBe("{date}/{name}");
    expect(v.includeProxies).toBe(false);
    expect(v.includeThumbnails).toBe(false);
    expect(v.verify).toBe(true);
    expect(v.spaceHeadroom).toBe(1073741824);
    expect(v.deleteAfterVerify).toBe(false);
    expect(v.autoEject).toBe(false);
    expect(v.cloud).toBeNull();
  });

  it("returns a fresh object each call (no shared mutation)", () => {
    const a = defaultConfigView("/a");
    const b = defaultConfigView("/b");
    a.verify = false;
    expect(b.verify).toBe(true);
    expect(a.destRoot).toBe("/a");
    expect(b.destRoot).toBe("/b");
  });
});

describe("withCloud", () => {
  const base = defaultConfigView("/Users/me/GPBeam");

  it("attaches a CloudView built from wizard cloud fields", () => {
    const cloud: CloudView = {
      destinationId: "nextcloud",
      baseUrl: "https://cloud.example.com",
      username: "alice",
      remoteRoot: "/GoPro",
      mirrorMode: "auto",
      chunkThreshold: 10485760,
      maxConcurrency: 2,
      maxAttempts: 5,
      hasPassword: true,
    };
    const v = withCloud(base, cloud);
    expect(v.cloud).toEqual(cloud);
    // base is not mutated
    expect(base.cloud).toBeNull();
  });

  it("withCloud(null) leaves cloud null", () => {
    const v = withCloud(base, null);
    expect(v.cloud).toBeNull();
  });
});
