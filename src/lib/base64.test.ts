import { afterEach, describe, expect, it, vi } from "vitest";
import { decodeBase64, decodeBase64Fallback } from "./base64";

const legacy = (b64: string) => Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
const encode = (bytes: number[]) => btoa(String.fromCharCode(...bytes));

const everyByte = Array.from({ length: 256 }, (_, i) => i);
const samples = [[], [0], [0x41, 0x42, 0x43], [0x80, 0xff, 0xfe, 0x00, 0x7f], everyByte];

type Native = { fromBase64?: unknown };

describe("decodeBase64", () => {
  afterEach(() => {
    delete (Uint8Array as unknown as Native).fromBase64;
  });

  it("fallback matches the old implementation, including empty and high bytes", () => {
    for (const s of samples) {
      const b64 = encode(s);
      expect(decodeBase64Fallback(b64)).toEqual(legacy(b64));
      expect(Array.from(decodeBase64Fallback(b64))).toEqual(s);
    }
  });

  it("uses the fallback when fromBase64 is absent", () => {
    delete (Uint8Array as unknown as Native).fromBase64;
    for (const s of samples) expect(Array.from(decodeBase64(encode(s)))).toEqual(s);
  });

  it("uses Uint8Array.fromBase64 when present", () => {
    const native = vi.fn((s: string) => legacy(s));
    (Uint8Array as unknown as Native).fromBase64 = native;
    const b64 = encode([0x80, 0x90, 0xff]);
    expect(Array.from(decodeBase64(b64))).toEqual([0x80, 0x90, 0xff]);
    expect(native).toHaveBeenCalledWith(b64);
    expect(decodeBase64("")).toEqual(new Uint8Array(0));
  });
});
