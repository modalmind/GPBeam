import { describe, it, expect } from "vitest";
import { defaultConfigView, defaultCloudView, withCloud, buildCloudView } from "./wizard_view";
import type { CloudView } from "../lib/bindings";

describe("defaultCloudView", () => {
  it("matches the Rust serde defaults (nc1, 50 MiB, concurrency 2, attempts 8, GoPro)", () => {
    const c = defaultCloudView();
    expect(c.destinationId).toBe("nc1");
    expect(c.baseUrl).toBe("");
    expect(c.username).toBe("");
    expect(c.remoteRoot).toBe("GoPro");
    expect(c.mirrorMode).toBe("off");
    expect(c.chunkThreshold).toBe(52428800);
    expect(c.maxConcurrency).toBe(2);
    expect(c.maxAttempts).toBe(8);
    expect(c.hasPassword).toBe(false);
  });

  it("returns a fresh object each call (no shared mutation)", () => {
    const a = defaultCloudView();
    a.destinationId = "mutated";
    expect(defaultCloudView().destinationId).toBe("nc1");
  });
});

describe("defaultConfigView", () => {
  it("mirrors the Rust M1 defaults with the given destination", () => {
    const v = defaultConfigView("/Users/me/GPBeam");
    expect(v.destRoot).toBe("/Users/me/GPBeam");
    expect(v.filenameTemplate).toBe("{date}_{original}");
    expect(v.includeProxies).toBe(false);
    expect(v.includeThumbnails).toBe(false);
    expect(v.verify).toBe(true);
    expect(v.spaceHeadroom).toBe(1073741824);
    expect(v.deleteAfterVerify).toBe(false);
    expect(v.autoEject).toBe(false);
    expect(v.cloud).toBeNull();
  });

  it("defaults wiredIngest to true (USB GoPro offload on)", () => {
    const v = defaultConfigView("/Users/me/GPBeam");
    expect(v.wiredIngest).toBe(true);
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
      destinationId: "nc1",
      baseUrl: "https://cloud.example.com",
      username: "alice",
      remoteRoot: "/GoPro",
      mirrorMode: "auto",
      chunkThreshold: 52428800,
      maxConcurrency: 2,
      maxAttempts: 8,
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

describe("buildCloudView", () => {
  const fields = {
    baseUrl: "https://cloud.example.com",
    username: "alice",
    appPassword: "secret-token",
    remoteRoot: "/GoPro",
    mirrorMode: "auto" as const,
  };

  it("builds a CloudView on the shared Rust-default advanced values and hasPassword=true", () => {
    const cv = buildCloudView(fields);
    expect(cv).not.toBeNull();
    // destinationId/advanced values come from defaultCloudView so the wizard and
    // CloudTab key the keychain entry identically and match the Rust serde defaults.
    expect(cv!.destinationId).toBe("nc1");
    expect(cv!.baseUrl).toBe("https://cloud.example.com");
    expect(cv!.username).toBe("alice");
    expect(cv!.remoteRoot).toBe("/GoPro");
    expect(cv!.mirrorMode).toBe("auto");
    expect(cv!.chunkThreshold).toBe(52428800);
    expect(cv!.maxConcurrency).toBe(2);
    expect(cv!.maxAttempts).toBe(8);
    expect(cv!.hasPassword).toBe(true);
  });

  it("hasPassword is false when the app-password is blank", () => {
    const cv = buildCloudView({ ...fields, appPassword: "  " });
    expect(cv!.hasPassword).toBe(false);
  });

  it("returns null when the base URL is blank (cloud skipped)", () => {
    expect(buildCloudView({ ...fields, baseUrl: "   " })).toBeNull();
  });

  it("trims base URL, username, and remote root", () => {
    const cv = buildCloudView({
      ...fields,
      baseUrl: "  https://x.test  ",
      username: " bob ",
      remoteRoot: " /a ",
    });
    expect(cv!.baseUrl).toBe("https://x.test");
    expect(cv!.username).toBe("bob");
    expect(cv!.remoteRoot).toBe("/a");
  });
});
