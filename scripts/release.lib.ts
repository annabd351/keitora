// Pure, side-effect-free logic for the release tool. Everything here is unit-tested
// (scripts/release.lib.test.ts). IO / process orchestration lives in scripts/release.ts.

export type BumpLevel = "major" | "minor" | "patch";

export interface ReleaseOptions {
  bump?: BumpLevel;
  setVersion?: string;
  overwrite: boolean;
  dryRun: boolean;
  help: boolean;
}

const SEMVER_RE = /^(\d+)\.(\d+)\.(\d+)$/;

/** Parse a strict `X.Y.Z` version into a numeric tuple, throwing on anything else. */
export function parseVersion(v: string): [number, number, number] {
  const m = SEMVER_RE.exec(v.trim());
  if (!m) throw new Error(`Invalid version "${v}": expected X.Y.Z`);
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

/** Increment a semver string by the given level (minor resets patch; major resets minor+patch). */
export function bumpVersion(current: string, level: BumpLevel): string {
  const [major, minor, patch] = parseVersion(current);
  switch (level) {
    case "major":
      return `${major + 1}.0.0`;
    case "minor":
      return `${major}.${minor + 1}.0`;
    case "patch":
      return `${major}.${minor}.${patch + 1}`;
  }
}

/** Validate/normalize an explicit `--set` version. */
export function parseSetVersion(v: string): string {
  const [major, minor, patch] = parseVersion(v);
  return `${major}.${minor}.${patch}`;
}

/** Compare two semver strings: negative if a<b, 0 if equal, positive if a>b. */
export function compareVersions(a: string, b: string): number {
  const pa = parseVersion(a);
  const pb = parseVersion(b);
  for (let i = 0; i < 3; i++) {
    if (pa[i] !== pb[i]) return pa[i] - pb[i];
  }
  return 0;
}

/**
 * Map Node's `process.arch` to the token Tauri embeds in bundle filenames.
 * Confirmed against the real NSIS artifact `Keitora_0.1.0_x64-setup.exe` (token `x64`),
 * and Tauri's macOS dmg naming (`aarch64` for Apple Silicon, `x64` for Intel).
 */
export function tauriArchToken(nodeArch: string): string {
  switch (nodeArch) {
    case "arm64":
      return "aarch64";
    case "x64":
      return "x64";
    default:
      throw new Error(`Unsupported arch "${nodeArch}" (expected arm64 or x64)`);
  }
}

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Regex matching the exact bundle artifact for this platform+version+arch.
 * macOS: `Keitora_{version}_{arch}.dmg`; Windows: `Keitora_{version}_{arch}-setup.exe`.
 * Deliberately anchored so stale pre-rename files (e.g. `OldName_0.1.0_x64-setup.exe`) never match.
 */
export function platformArtifactPattern(
  platform: string,
  version: string,
  archToken: string,
): RegExp {
  const v = escapeRegex(version);
  const a = escapeRegex(archToken);
  if (platform === "darwin") return new RegExp(`^Keitora_${v}_${a}\\.dmg$`);
  if (platform === "win32") return new RegExp(`^Keitora_${v}_${a}-setup\\.exe$`);
  throw new Error(`Unsupported platform "${platform}" (expected darwin or win32)`);
}

export function assetMatches(assetName: string, pattern: RegExp): boolean {
  return pattern.test(assetName);
}

export type UploadDecision = "skip" | "clobber" | "upload";

/** Decide what to do given whether the release already has this platform's asset. */
export function decideUpload(assetPresent: boolean, overwrite: boolean): UploadDecision {
  if (!assetPresent) return "upload";
  return overwrite ? "clobber" : "skip";
}

/**
 * Whether a command must be spawned through a shell. On Windows, `yarn` is a
 * `.cmd` shim that Node's CreateProcess (which only appends `.exe`) cannot find
 * without a shell; `git`/`gh`/`cargo`/`bash` are real executables and resolve
 * directly. Spawning through a shell re-joins args WITHOUT re-quoting, so it must
 * be avoided for any command carrying arguments with spaces (commit messages,
 * file paths) — those are the ones we keep shell-free.
 */
export function needsShell(cmd: string, platform: string): boolean {
  return platform === "win32" && cmd === "yarn";
}

/** Parse argv (already sliced past node + script). Throws on conflicting/unknown flags. */
export function parseArgs(argv: string[]): ReleaseOptions {
  const opts: ReleaseOptions = { overwrite: false, dryRun: false, help: false };
  const bumpFlags: BumpLevel[] = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    switch (arg) {
      case "--major":
      case "--minor":
      case "--patch":
        bumpFlags.push(arg.slice(2) as BumpLevel);
        break;
      case "--set": {
        const next = argv[i + 1];
        if (!next || next.startsWith("--")) throw new Error("--set requires a version argument (X.Y.Z)");
        opts.setVersion = parseSetVersion(next);
        i++;
        break;
      }
      case "--overwrite":
        opts.overwrite = true;
        break;
      case "--dry-run":
        opts.dryRun = true;
        break;
      case "--help":
      case "-h":
        opts.help = true;
        break;
      default:
        throw new Error(`Unknown argument "${arg}"`);
    }
  }
  if (bumpFlags.length > 1) {
    throw new Error(`Conflicting bump flags: ${bumpFlags.map((b) => `--${b}`).join(" ")}`);
  }
  if (bumpFlags.length === 1 && opts.setVersion) {
    throw new Error("Cannot combine --set with --major/--minor/--patch");
  }
  if (bumpFlags.length === 1) opts.bump = bumpFlags[0];
  return opts;
}
