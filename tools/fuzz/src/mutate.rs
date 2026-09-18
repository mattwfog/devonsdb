use crate::rng::Rng;

const INTERESTING: [u8; 16] = [
    0, 1, 2, 7, 8, 15, 16, 31, 32, b'0', b'9', b'"', b'{', b'}', 0x7f, 0xff,
];

/// Applies bounded byte mutations while never growing beyond `cap`.
pub(crate) fn mutate_bytes(bytes: &mut Vec<u8>, rng: &mut Rng, rounds: usize, cap: usize) {
    for _ in 0..rounds.max(1) {
        match rng.index(6) {
            0 => flip_bit(bytes, rng),
            1 => overwrite(bytes, rng),
            2 => insert(bytes, rng, cap),
            3 => delete(bytes, rng),
            4 => duplicate(bytes, rng, cap),
            _ => swap(bytes, rng),
        }
    }
    if bytes.len() > cap {
        bytes.truncate(cap);
    }
}

fn flip_bit(bytes: &mut [u8], rng: &mut Rng) {
    if bytes.is_empty() {
        return;
    }
    let index = rng.index(bytes.len());
    bytes[index] ^= 1 << rng.index(8);
}

fn overwrite(bytes: &mut Vec<u8>, rng: &mut Rng) {
    if bytes.is_empty() {
        bytes.push(INTERESTING[rng.index(INTERESTING.len())]);
        return;
    }
    let start = rng.index(bytes.len());
    let count = rng.range(1, 16).min(bytes.len() - start);
    let value = INTERESTING[rng.index(INTERESTING.len())];
    bytes[start..start + count].fill(value);
}

fn insert(bytes: &mut Vec<u8>, rng: &mut Rng, cap: usize) {
    let available = cap.saturating_sub(bytes.len());
    if available == 0 {
        return;
    }
    let count = rng.range(1, 32).min(available);
    let index = rng.index(bytes.len().saturating_add(1));
    let inserted: Vec<u8> = (0..count).map(|_| rng.byte()).collect();
    bytes.splice(index..index, inserted);
}

fn delete(bytes: &mut Vec<u8>, rng: &mut Rng) {
    if bytes.is_empty() {
        return;
    }
    let start = rng.index(bytes.len());
    let count = rng.range(1, 32).min(bytes.len() - start);
    bytes.drain(start..start + count);
}

fn duplicate(bytes: &mut Vec<u8>, rng: &mut Rng, cap: usize) {
    if bytes.is_empty() || bytes.len() >= cap {
        return;
    }
    let start = rng.index(bytes.len());
    let count = rng
        .range(1, 32)
        .min(bytes.len() - start)
        .min(cap - bytes.len());
    let copy = bytes[start..start + count].to_vec();
    let target = rng.index(bytes.len().saturating_add(1));
    bytes.splice(target..target, copy);
}

fn swap(bytes: &mut [u8], rng: &mut Rng) {
    if bytes.len() < 2 {
        return;
    }
    let left = rng.index(bytes.len());
    let right = rng.index(bytes.len());
    bytes.swap(left, right);
}
