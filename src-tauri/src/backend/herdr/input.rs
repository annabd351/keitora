//! `input` — pure translation of raw xterm keystroke bytes into herdr input ops
//! (the "B2" layer).
//!
//! herdr's input API is a **key-name abstraction, not raw-byte passthrough**
//! (per the herdr CLI): literal text goes through
//! `pane send-text`, and named keys through `pane send-keys` (canonical names
//! verified against herdr 0.9.3: `enter`, `up`, `down`, `left`, `right`,
//! `backspace`, `tab`, `esc`, `ctrl+c` — note `ctrl+c`, **not** `ctrl-c`, which
//! herdr rejects as `invalid_key`).
//!
//! [`translate_input`] turns a byte run (one `term.onData` chunk) into an ordered
//! list of [`HerdrInput`] ops, preserving order so `"ab\r"` dispatches as text
//! `"ab"` then an `enter` key.
//!
//! TODO: exotic-key fidelity (full kitty-keyboard range, unusual
//! modifier combos, IME, bracketed paste) is deferred to the human sign-off;
//! this covers the common interactive cases only.

/// One herdr input operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrInput {
    /// Literal UTF-8 text → `pane send-text` (no trailing Enter).
    Text(String),
    /// One or more named key presses → `pane send-keys`.
    Keys(Vec<String>),
}

/// Flush any accumulated text run into the output list.
fn flush(text: &mut String, out: &mut Vec<HerdrInput>) {
    if !text.is_empty() {
        out.push(HerdrInput::Text(std::mem::take(text)));
    }
}

/// Translate a chunk of raw keystroke bytes into ordered herdr input ops.
///
/// Mapping:
/// - printable UTF-8 runs (`>= 0x20`, excluding `0x7f`) → [`HerdrInput::Text`];
/// - `\r` / `\n` → `enter`;
/// - `ESC [ A|B|C|D` → `up`/`down`/`right`/`left`;
/// - `0x7f` / `0x08` → `backspace`;
/// - `\t` → `tab`;
/// - a lone `ESC` (or an unrecognised `ESC [ …`) → `esc`;
/// - `0x03` → `ctrl+c`;
/// - any other control byte → skipped (best-effort; logged by the caller via the
///   fact that it produces no op).
pub fn translate_input(bytes: &[u8]) -> Vec<HerdrInput> {
    let mut out: Vec<HerdrInput> = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                // Arrow keys in BOTH forms: CSI (`ESC [ A/B/C/D`, cursor-key mode
                // off) and SS3 (`ESC O A/B/C/D`, application-cursor mode on — what
                // readline/editors emit). Both introducers map the same final byte.
                if i + 2 < bytes.len() && (bytes[i + 1] == b'[' || bytes[i + 1] == b'O') {
                    let named = match bytes[i + 2] {
                        b'A' => Some("up"),
                        b'B' => Some("down"),
                        b'C' => Some("right"),
                        b'D' => Some("left"),
                        _ => None,
                    };
                    if let Some(name) = named {
                        flush(&mut text, &mut out);
                        out.push(HerdrInput::Keys(vec![name.to_string()]));
                        i += 3;
                        continue;
                    }
                }
                // Lone ESC or an unrecognised escape introducer → esc.
                flush(&mut text, &mut out);
                out.push(HerdrInput::Keys(vec!["esc".to_string()]));
                i += 1;
            }
            b'\r' | b'\n' => {
                flush(&mut text, &mut out);
                out.push(HerdrInput::Keys(vec!["enter".to_string()]));
                // Collapse a CRLF pair into a SINGLE enter (a `\r\n` chunk must not
                // submit twice).
                i += if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    2
                } else {
                    1
                };
            }
            0x08 | 0x7f => {
                flush(&mut text, &mut out);
                out.push(HerdrInput::Keys(vec!["backspace".to_string()]));
                i += 1;
            }
            b'\t' => {
                flush(&mut text, &mut out);
                out.push(HerdrInput::Keys(vec!["tab".to_string()]));
                i += 1;
            }
            0x03 => {
                flush(&mut text, &mut out);
                out.push(HerdrInput::Keys(vec!["ctrl+c".to_string()]));
                i += 1;
            }
            _ if b >= 0x20 && b != 0x7f => {
                // Printable run: ASCII printable plus any UTF-8 multibyte bytes.
                let start = i;
                while i < bytes.len() && bytes[i] >= 0x20 && bytes[i] != 0x7f {
                    i += 1;
                }
                text.push_str(&String::from_utf8_lossy(&bytes[start..i]));
            }
            _ => {
                // Unknown control byte: best-effort skip (exotic-key fidelity is deferred).
                i += 1;
            }
        }
    }
    flush(&mut text, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(ks: &[&str]) -> HerdrInput {
        HerdrInput::Keys(ks.iter().map(|s| s.to_string()).collect())
    }
    fn text(s: &str) -> HerdrInput {
        HerdrInput::Text(s.to_string())
    }

    #[test]
    fn printable_run_is_one_text_op() {
        assert_eq!(translate_input(b"hello world"), vec![text("hello world")]);
    }

    #[test]
    fn enter_from_cr_and_lf() {
        assert_eq!(translate_input(b"\r"), vec![keys(&["enter"])]);
        assert_eq!(translate_input(b"\n"), vec![keys(&["enter"])]);
    }

    #[test]
    fn text_then_enter_preserves_order() {
        assert_eq!(translate_input(b"ab\r"), vec![text("ab"), keys(&["enter"])]);
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(translate_input(b"\x1b[A"), vec![keys(&["up"])]);
        assert_eq!(translate_input(b"\x1b[B"), vec![keys(&["down"])]);
        assert_eq!(translate_input(b"\x1b[C"), vec![keys(&["right"])]);
        assert_eq!(translate_input(b"\x1b[D"), vec![keys(&["left"])]);
    }

    #[test]
    fn backspace_del_and_bs() {
        assert_eq!(translate_input(b"\x7f"), vec![keys(&["backspace"])]);
        assert_eq!(translate_input(b"\x08"), vec![keys(&["backspace"])]);
    }

    #[test]
    fn tab_and_esc_and_ctrl_c() {
        assert_eq!(translate_input(b"\t"), vec![keys(&["tab"])]);
        assert_eq!(translate_input(b"\x1b"), vec![keys(&["esc"])]);
        assert_eq!(translate_input(b"\x03"), vec![keys(&["ctrl+c"])]);
    }

    #[test]
    fn ss3_application_cursor_arrows() {
        // ESC O A..D (application-cursor mode) map to the same arrows as CSI.
        assert_eq!(translate_input(b"\x1bOA"), vec![keys(&["up"])]);
        assert_eq!(translate_input(b"\x1bOB"), vec![keys(&["down"])]);
        assert_eq!(translate_input(b"\x1bOC"), vec![keys(&["right"])]);
        assert_eq!(translate_input(b"\x1bOD"), vec![keys(&["left"])]);
    }

    #[test]
    fn crlf_is_a_single_enter() {
        assert_eq!(translate_input(b"\r\n"), vec![keys(&["enter"])]);
        assert_eq!(
            translate_input(b"ab\r\n"),
            vec![text("ab"), keys(&["enter"])]
        );
        // a bare LF after text is still one enter
        assert_eq!(translate_input(b"x\n"), vec![text("x"), keys(&["enter"])]);
    }

    #[test]
    fn mixed_sequence_roundtrips_in_order() {
        // type "ls", up-arrow (history), enter
        assert_eq!(
            translate_input(b"ls\x1b[A\r"),
            vec![text("ls"), keys(&["up"]), keys(&["enter"])]
        );
    }

    #[test]
    fn utf8_multibyte_is_preserved_as_text() {
        assert_eq!(translate_input("café→".as_bytes()), vec![text("café→")]);
    }

    #[test]
    fn unknown_control_byte_is_skipped() {
        // 0x01 (ctrl-a) is not mapped for the slice → produces no op.
        assert_eq!(translate_input(b"\x01"), vec![]);
        // …and surrounding text still lands: the skipped byte doesn't flush the
        // run, so the neighbours coalesce into one text op.
        assert_eq!(translate_input(b"a\x01b"), vec![text("ab")]);
    }
}
