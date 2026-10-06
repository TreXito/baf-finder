//! Generate a key for the public flip feed (`public_ws.rs`).
//!
//! Prints the secret ONCE, plus the `public-keys.json` entry to paste. The entry
//! carries only the SHA-256 digest, so the plaintext never lands on the server:
//! if the key file leaks, nobody gains access, and a lost key is replaced rather
//! than recovered.
//!
//!     cargo run --bin public_key -- alice
//!
//! 128 bits from the OS CSPRNG, base32-ish alphabet (no look-alike characters),
//! so it survives being pasted into a URL, a chat message, or a config file.

use ring::rand::SecureRandom;

/// No 0/O/1/I/l — a key gets copied by humans out of chat at least once.
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

fn main() {
    let label = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "unnamed".to_string());
    let rng = ring::rand::SystemRandom::new();
    // 26 chars over a 31-symbol alphabet ≈ 128 bits.
    let mut raw = [0u8; 64];
    rng.fill(&mut raw)
        .expect("OS CSPRNG unavailable — refusing to emit a weak key");
    // Rejection sampling: `% 31` over a 0..=255 byte would bias the low symbols.
    // 248 = 31 * 8 is the largest multiple of 31 that fits in a byte.
    let secret: String = raw
        .iter()
        .filter(|b| **b < 248)
        .take(26)
        .map(|b| ALPHABET[(*b % ALPHABET.len() as u8) as usize] as char)
        .collect();
    assert_eq!(
        secret.len(),
        26,
        "not enough entropy survived rejection sampling — rerun"
    );

    let digest = ring::digest::digest(&ring::digest::SHA256, secret.as_bytes());
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();

    println!("secret (shown once — give this to '{label}', it is NOT recoverable):\n");
    println!("    {secret}\n");
    println!("connect URL:\n");
    println!("    wss://your.host/{secret}\n");
    println!("public-keys.json entry:\n");
    println!("    {{");
    println!("      \"label\": \"{label}\",");
    println!("      \"sha256\": \"{hex}\",");
    println!("      \"enabled\": true,");
    println!("      \"maxConnections\": 3");
    println!("    }}");
}
