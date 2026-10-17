// Wording for the one-time notice that tools running in panes now use the
// user's normal folders instead of Keitora's isolated ones. Pure, so the text is
// unit-testable; App shows it in a confirm dialog that renders newlines.

/**
 * The notice body for the old tool folders left in place (`dirs`, relative to
 * the old isolated root, shown one bullet per line in the given order). The root
 * is always written as the default `~/.keitora/herdr`.
 */
export function xdgLeakNoticeBody(dirs: string[]): string {
  const bullets = dirs.map((d) => `  • ${d}`).join("\n");
  return [
    "Tools running inside Keitora panes (like gh and claude) used to save their settings, caches and logins under ~/.keitora/herdr. They now use your normal locations, the same as a regular terminal.",
    "",
    "Because of that, gh may look logged out in new panes. Run:  gh auth login",
    "",
    "Nothing was copied or deleted. The old folders are still there:",
    bullets,
    "(under ~/.keitora/herdr). Sessions that are already open keep the old locations until you restart them.",
  ].join("\n");
}
