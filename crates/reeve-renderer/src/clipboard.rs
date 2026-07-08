//! System clipboard via OSC 52: a pure escape sequence the terminal
//! translates to the local clipboard. Chosen over a native clipboard
//! library because Reeve plausibly runs over SSH, where there is no
//! display to reach; OSC 52 works through SSH and degrades to a silent
//! no-op on terminals that refuse it, and it adds no native build deps.

use std::io::Write;

/// Copies `text` to the system clipboard. Emitted raw even under tmux:
/// tmux understands OSC 52 natively and its set-clipboard option decides
/// whether to store the buffer, forward to the outer terminal, or both.
/// A passthrough envelope would do the opposite of what it suggests,
/// telling tmux not to interpret the sequence, and it is dropped whole
/// unless allow-passthrough is on, which it is not by default.
pub fn copy(text: &str) {
    let payload = base64(text.as_bytes());
    let seq = format!("\x1b]52;c;{payload}\x07");
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Standard base64, alphabet and padding per RFC 4648. Hand-rolled: the
/// whole algorithm is smaller than a dependency entry.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
