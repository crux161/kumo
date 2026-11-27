//J947_r13_001
//! The kernel CSPRNG (SH9-c8): ChaCha20 keyed from firmware entropy, read through
//! `Syscall::RandomRead`.
//!
//! Nijigumo asks the firmware's `EFI_RNG_PROTOCOL` for 32 bytes and passes them in BootInfo
//! (ABI v4). [`seed_from_boot`] takes them before any BootInfo copy reaches userland; the
//! key is SHA-256 over a domain label and the seed, so a biased firmware source still yields
//! a uniform key. [`fill`] uses fast key erasure: each request first replaces the key with
//! keystream it never returns, so a later compromise of the state cannot reproduce earlier
//! output. There is no second entropy source yet: a kernel the firmware gave nothing stays
//! unseeded and every read is refused. It never falls back to a clock or a counter. — TEAL

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use kumo_abi::boot::BootInfo;

/// The 20-round ChaCha block function (RFC 8439 §2.3): `key`, block `counter`, 96-bit `nonce`.
pub fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
    let word = |bytes: &[u8], index: usize| {
        u32::from_le_bytes([
            bytes[4 * index],
            bytes[4 * index + 1],
            bytes[4 * index + 2],
            bytes[4 * index + 3],
        ])
    };
    let mut initial = [0u32; 16];
    initial[..4].copy_from_slice(&SIGMA);
    for index in 0..8 {
        initial[4 + index] = word(key, index);
    }
    initial[12] = counter;
    for index in 0..3 {
        initial[13 + index] = word(nonce, index);
    }
    let mut state = initial;
    fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        s[a] = s[a].wrapping_add(s[b]);
        s[d] = (s[d] ^ s[a]).rotate_left(16);
        s[c] = s[c].wrapping_add(s[d]);
        s[b] = (s[b] ^ s[c]).rotate_left(12);
        s[a] = s[a].wrapping_add(s[b]);
        s[d] = (s[d] ^ s[a]).rotate_left(8);
        s[c] = s[c].wrapping_add(s[d]);
        s[b] = (s[b] ^ s[c]).rotate_left(7);
    }
    for _ in 0..10 {
        quarter(&mut state, 0, 4, 8, 12);
        quarter(&mut state, 1, 5, 9, 13);
        quarter(&mut state, 2, 6, 10, 14);
        quarter(&mut state, 3, 7, 11, 15);
        quarter(&mut state, 0, 5, 10, 15);
        quarter(&mut state, 1, 6, 11, 12);
        quarter(&mut state, 2, 7, 8, 13);
        quarter(&mut state, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for index in 0..16 {
        out[4 * index..4 * index + 4]
            .copy_from_slice(&state[index].wrapping_add(initial[index]).to_le_bytes());
    }
    out
}

/// Domain label hashed ahead of the firmware seed.
const KEY_LABEL: &[u8] = b"KUMO kernel CSPRNG v1 (J947_r13_001)";

struct Generator {
    key: [u8; 32],
    seeded: bool,
}

impl Generator {
    const fn new() -> Self {
        Self {
            key: [0; 32],
            seeded: false,
        }
    }

    fn seed(&mut self, seed: &[u8]) {
        let mut hash = kumo_sha256::Sha256::new();
        hash.update(KEY_LABEL);
        hash.update(seed);
        self.key = hash.finish();
        self.seeded = true;
    }

    /// Fast key erasure: block 0's first half becomes the next key (never output), its second
    /// half and every following block are the output. The nonce stays zero because no key is
    /// ever used for more than one request.
    fn fill(&mut self, dest: &mut [u8]) -> bool {
        if !self.seeded {
            return false;
        }
        const NONCE: [u8; 12] = [0; 12];
        let first = chacha20_block(&self.key, 0, &NONCE);
        let mut next_key = [0u8; 32];
        next_key.copy_from_slice(&first[..32]);
        let take = dest.len().min(32);
        dest[..take].copy_from_slice(&first[32..32 + take]);
        let mut done = take;
        let mut counter = 1u32;
        while done < dest.len() {
            let block = chacha20_block(&self.key, counter, &NONCE);
            let take = (dest.len() - done).min(64);
            dest[done..done + take].copy_from_slice(&block[..take]);
            done += take;
            counter += 1;
        }
        self.key = next_key;
        true
    }
}

struct GeneratorCell(UnsafeCell<Generator>);

// SAFETY: every access holds `BUSY`.
unsafe impl Sync for GeneratorCell {}

static GENERATOR: GeneratorCell = GeneratorCell(UnsafeCell::new(Generator::new()));
static BUSY: AtomicBool = AtomicBool::new(false);

fn with_generator<R>(work: impl FnOnce(&mut Generator) -> R) -> R {
    while BUSY
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: `BUSY` is held, so this is the only reference.
    let result = work(unsafe { &mut *GENERATOR.0.get() });
    BUSY.store(false, Ordering::Release);
    result
}

/// Seed from the loader's firmware entropy, if it passed any. Returns whether the kernel is
/// seeded. Callers must still drop the seed from every BootInfo copy they hand on
/// ([`BootInfo::forget_rng_seed`]).
pub fn seed_from_boot(boot: &BootInfo) -> bool {
    if boot.version == kumo_abi::ABI_VERSION && boot.has_rng_seed() {
        let seed = boot.rng_seed;
        with_generator(|generator| generator.seed(&seed));
    }
    is_seeded()
}

/// Whether firmware entropy keyed the generator this boot.
pub fn is_seeded() -> bool {
    with_generator(|generator| generator.seeded)
}

/// Fill `dest` with CSPRNG output; `false` (and `dest` untouched) when unseeded.
pub fn fill(dest: &mut [u8]) -> bool {
    with_generator(|generator| generator.fill(dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8439 §2.3.2, cross-checked with `openssl enc -chacha20` on the host.
    #[test]
    fn chacha20_block_matches_rfc_8439() {
        let key: [u8; 32] = core::array::from_fn(|index| index as u8);
        let nonce = [0, 0, 0, 0x09, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let expected = "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
                        d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e";
        let block = chacha20_block(&key, 1, &nonce);
        let hex: alloc::string::String = block
            .iter()
            .map(|byte| alloc::format!("{byte:02x}"))
            .collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn unseeded_refuses_and_seeded_never_repeats() {
        let mut generator = Generator::new();
        let mut out = [7u8; 40];
        assert!(!generator.fill(&mut out));
        assert_eq!(out, [7u8; 40], "an unseeded read must not touch the buffer");
        generator.seed(&[0x5a; 32]);
        let mut first = [0u8; 100];
        let mut second = [0u8; 100];
        assert!(generator.fill(&mut first));
        assert!(generator.fill(&mut second));
        assert_ne!(
            first, second,
            "fast key erasure: a request never replays the last one"
        );
        // Deterministic for a given seed, and the output never contains the next key.
        let mut again = Generator::new();
        again.seed(&[0x5a; 32]);
        let mut replay = [0u8; 100];
        assert!(again.fill(&mut replay));
        assert_eq!(first, replay);
        assert_ne!(&first[..32], &again.key[..]);
    }

    #[test]
    fn seed_from_boot_takes_only_a_flagged_current_abi_seed() {
        let mut boot = BootInfo::empty(kumo_abi::ABI_VERSION);
        boot.rng_seed = [1; 32];
        let mut generator = Generator::new();
        // Without the flag nothing is taken; `seed_from_boot` itself is global, so exercise
        // the same predicate it uses.
        assert!(!(boot.version == kumo_abi::ABI_VERSION && boot.has_rng_seed()));
        boot.flags |= BootInfo::FLAG_RNG_SEED_PRESENT;
        assert!(boot.version == kumo_abi::ABI_VERSION && boot.has_rng_seed());
        generator.seed(&boot.rng_seed);
        assert!(generator.seeded);
        let mut snapshot = boot;
        snapshot.forget_rng_seed();
        assert_eq!(snapshot.rng_seed, [0; 32]);
        assert!(!snapshot.has_rng_seed());
    }
}
