// Shared base64 decode for PTY output (host batches output base64-encoded).
// Extracted to a dependency-free module so both ipc.ts and transport.ts can share
// one implementation without an import cycle (transport ↔ ipc).

type NativeBase64 = { fromBase64?: (s: string) => Uint8Array };

/** `atob` plus an indexed copy into a preallocated array (no per-char closure). */
export function decodeBase64Fallback(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** Decode a base64 string to raw bytes (for xterm.write). */
export function decodeBase64(b64: string): Uint8Array {
  // `Uint8Array.fromBase64` is not in every runtime or in the TS lib types yet.
  const native = (Uint8Array as unknown as NativeBase64).fromBase64;
  if (typeof native === "function") return native.call(Uint8Array, b64);
  return decodeBase64Fallback(b64);
}
