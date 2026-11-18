//! Fractional index keys for the tab strip's order (plans/08a §3, §7.14):
//! strings over `[0-9a-z]`, at most [`MAX_ORDER_KEY_CHARS`] long, compared
//! byte-wise (which is also how the server and the web UI compare them). A
//! tab moves by getting a key between its new neighbours' keys, so a move on
//! one computer touches one record and merges with moves elsewhere.
//!
//! A key is read as a base-36 fraction `0.d₁d₂…`; missing digits are 0. ASCII
//! puts `'0'..='9'` before `'a'..='z'`, so byte order is numeric order.
//!
//! ## References
//! * The midpoint construction follows "Implementing Fractional Indexing"
//!   (David Greenspan, Figma, 2020): <https://observablehq.com/@dgreensp/implementing-fractional-indexing>

use super::tabs::MAX_ORDER_KEY_CHARS;

const BASE: u8 = 36;

fn digit_at(key: &str, i: usize) -> u8 {
    key.as_bytes().get(i).map_or(0, |&b| match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'z' => b - b'a' + 10,
        // Not a key character: treated as the lowest digit. Callers only pass
        // keys that passed `valid_order_key`; this keeps the function total.
        _ => 0,
    })
}

fn char_of(d: u8) -> char {
    char::from(if d < 10 { b'0' + d } else { b'a' + d - 10 })
}

/// A key strictly between `lo` and `hi` (`None` = the start / the end of the
/// strip), as short as possible. `None` when there is no such key within
/// [`MAX_ORDER_KEY_CHARS`] characters (the neighbours are too close, or not in
/// order): the caller then gives the whole strip fresh keys with [`spread`].
pub fn between(lo: Option<&str>, hi: Option<&str>) -> Option<String> {
    let lo = lo.unwrap_or("");
    let mut out = String::new();
    // Once a digit below `hi`'s is taken, everything after is below `hi`.
    let mut below_hi = hi.is_none();
    for i in 0..MAX_ORDER_KEY_CHARS {
        let l = digit_at(lo, i);
        let h = if below_hi {
            BASE
        } else {
            digit_at(hi.unwrap_or(""), i)
        };
        if h < l {
            return None; // `hi` is not above `lo`
        }
        if h - l >= 2 {
            out.push(char_of(l + (h - l) / 2));
            return in_order(lo, &out, hi).then_some(out);
        }
        out.push(char_of(l));
        if h - l == 1 {
            below_hi = true;
        }
    }
    None
}

fn in_order(lo: &str, key: &str, hi: Option<&str>) -> bool {
    (lo.is_empty() || lo < key) && hi.is_none_or(|h| key < h)
}

/// `n` keys in ascending order, evenly spread over the key space, all of the
/// same (smallest sufficient) length — for a strip whose keys ran out of room
/// between two neighbours, or for tabs that never had one.
pub fn spread(n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    // Smallest width whose key space has room for n keys with gaps.
    let mut width = 1u32;
    while (BASE as u128).pow(width) <= n as u128 + 1 {
        width += 1;
    }
    let space = (BASE as u128).pow(width);
    let step = space / (n as u128 + 1);
    (1..=n as u128)
        .map(|i| {
            let mut v = i * step;
            let mut digits = vec![0u8; width as usize];
            for d in digits.iter_mut().rev() {
                *d = (v % BASE as u128) as u8;
                v /= BASE as u128;
            }
            digits.into_iter().map(char_of).collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::valid_order_key;

    #[test]
    fn a_key_between_is_strictly_between_and_short() {
        assert_eq!(between(None, None).as_deref(), Some("i"));
        assert_eq!(between(Some("i"), None).as_deref(), Some("r"));
        assert_eq!(between(None, Some("i")).as_deref(), Some("9"));
        assert_eq!(between(Some("a"), Some("b")).as_deref(), Some("ai"));
        assert_eq!(between(Some("a"), Some("c")).as_deref(), Some("b"));
        assert_eq!(between(Some("az"), Some("b")).as_deref(), Some("azi"));
        for (lo, hi) in [
            (None, None),
            (Some("0"), Some("01")),
            (Some("a"), Some("a01")),
            (Some("zz"), None),
            (None, Some("01")),
        ] {
            let k = between(lo, hi).unwrap();
            assert!(valid_order_key(&k), "{k}");
            assert!(lo.is_none_or(|lo| lo < k.as_str()), "{lo:?} < {k}");
            assert!(hi.is_none_or(|hi| k.as_str() < hi), "{k} < {hi:?}");
        }
    }

    #[test]
    fn no_key_when_the_neighbours_leave_no_room_or_are_reversed() {
        assert_eq!(between(Some("b"), Some("a")), None);
        assert_eq!(between(Some("a"), Some("a")), None);
        // "a" and "a0" are the same fraction: nothing lies between.
        assert_eq!(between(Some("a"), Some("a0")), None);
        // Room only beyond the length limit.
        let lo = "a".repeat(MAX_ORDER_KEY_CHARS);
        let hi = format!("{}b", "a".repeat(MAX_ORDER_KEY_CHARS - 1));
        assert_eq!(between(Some(&lo), Some(&hi)), None);
    }

    #[test]
    fn repeated_inserts_at_one_spot_stay_ordered_until_the_limit() {
        // Always inserting right after "a": keys grow by about one character
        // per step, and stay valid and ordered until they run out.
        let mut hi = String::from("b");
        let mut steps = 0;
        while let Some(k) = between(Some("a"), Some(&hi)) {
            assert!(valid_order_key(&k));
            assert!("a" < k.as_str() && k < hi);
            hi = k;
            steps += 1;
        }
        assert!(steps >= 20, "only {steps} inserts fit");
    }

    #[test]
    fn spread_keys_ascend_with_room_between() {
        for n in [1, 2, 35, 36, 100, 1000] {
            let keys = spread(n);
            assert_eq!(keys.len(), n);
            for w in keys.windows(2) {
                assert!(w[0] < w[1]);
                assert!(between(Some(&w[0]), Some(&w[1])).is_some());
            }
            assert!(keys.iter().all(|k| valid_order_key(k)));
            assert!(between(None, Some(&keys[0])).is_some());
            assert!(between(keys.last().map(String::as_str), None).is_some());
        }
        assert!(spread(0).is_empty());
    }
}
