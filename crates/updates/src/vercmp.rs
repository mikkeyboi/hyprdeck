//! Package version comparison with libalpm's `alpm_pkg_vercmp` semantics
//! (`[epoch:]pkgver[-pkgrel]`, rpmvercmp segment rules).

use std::cmp::Ordering;

/// Compare two full package versions exactly like `/usr/bin/vercmp`.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (e1, v1, r1) = parse_evr(a);
    let (e2, v2, r2) = parse_evr(b);
    rpmvercmp(e1, e2)
        .then_with(|| rpmvercmp(v1, v2))
        .then_with(|| match (r1, r2) {
            (Some(r1), Some(r2)) => rpmvercmp(r1, r2),
            _ => Ordering::Equal,
        })
}

/// Split `[epoch:]version[-release]` the way libalpm's `parseEVR` does.
fn parse_evr(evr: &str) -> (&str, &str, Option<&str>) {
    let digits = evr.bytes().take_while(u8::is_ascii_digit).count();
    let (epoch, rest) = if evr.as_bytes().get(digits) == Some(&b':') {
        let e = &evr[..digits];
        (if e.is_empty() { "0" } else { e }, &evr[digits + 1..])
    } else {
        ("0", evr)
    };
    // libalpm splits the release at the last '-'.
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], Some(&rest[i + 1..])),
        None => (epoch, rest, None),
    }
}

/// The upstream part of a package version: strips `epoch:` and `-pkgrel`.
pub fn pkgver(full: &str) -> &str {
    parse_evr(full).1
}

/// rpmvercmp: compare alternating numeric/alphabetic segments.
fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut one, mut two) = (0usize, 0usize);
    let (mut p1, mut p2) = (0usize, 0usize);
    while one < a.len() && two < b.len() {
        while one < a.len() && !a[one].is_ascii_alphanumeric() {
            one += 1;
        }
        while two < b.len() && !b[two].is_ascii_alphanumeric() {
            two += 1;
        }
        if one >= a.len() || two >= b.len() {
            break;
        }
        // Different separator lengths decide the comparison.
        if one - p1 != two - p2 {
            return (one - p1).cmp(&(two - p2));
        }
        p1 = one;
        p2 = two;
        let isnum = a[p1].is_ascii_digit();
        if isnum {
            while p1 < a.len() && a[p1].is_ascii_digit() {
                p1 += 1;
            }
            while p2 < b.len() && b[p2].is_ascii_digit() {
                p2 += 1;
            }
        } else {
            while p1 < a.len() && a[p1].is_ascii_alphabetic() {
                p1 += 1;
            }
            while p2 < b.len() && b[p2].is_ascii_alphabetic() {
                p2 += 1;
            }
        }
        if two == p2 {
            // Segment types differ: numeric beats alpha.
            return if isnum {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (mut s1, mut s2) = (&a[one..p1], &b[two..p2]);
        if isnum {
            while s1.first() == Some(&b'0') {
                s1 = &s1[1..];
            }
            while s2.first() == Some(&b'0') {
                s2 = &s2[1..];
            }
            match s1.len().cmp(&s2.len()) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        match s1.cmp(s2) {
            Ordering::Equal => {}
            other => return other,
        }
        one = p1;
        two = p2;
    }
    let (rest1, rest2) = (&a[one.min(a.len())..], &b[two.min(b.len())..]);
    if rest1.is_empty() && rest2.is_empty() {
        return Ordering::Equal;
    }
    // A remaining alpha segment never beats an empty string.
    let two_alpha = rest2.first().is_some_and(u8::is_ascii_alphabetic);
    let one_alpha = rest1.first().is_some_and(u8::is_ascii_alphabetic);
    if (rest1.is_empty() && !two_alpha) || one_alpha {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference results produced by `/usr/bin/vercmp a b` (pacman 7).
    const CASES: &[(&str, &str, i8)] = &[
        ("1.0", "1.0", 0),
        ("1.0", "1.0a", 1),
        ("1.0a", "1.0", -1),
        ("1.0.a", "1.0", 1),
        ("1.0", "1.0.a", -1),
        ("1.0alpha", "1.0", -1),
        ("1.0", "1.0beta", 1),
        ("1.0.0", "1.0", 1),
        ("1.0", "1.0.0", -1),
        ("1.0.1", "1.0", 1),
        ("1.0", "1.0.1", -1),
        ("1:1.0", "2.0", 1),
        ("1.0-1", "1.0-2", -1),
        ("1.0", "1.0-2", 0),
        ("1.0-1", "1.0", 0),
        ("1.0..0", "1.0.0", 1),
        ("1.0__0", "1.0.0", 1),
        ("1.0.a", "1.0.1", -1),
        ("1.0a1", "1.0", -1),
        ("1.0", "1.0a1", 1),
        ("1.0rc1", "1.0", -1),
        ("5.2.1.r12.gabc", "5.2.1", 1),
        (
            "0.0.45_nightly.20260930.2510",
            "0.0.46_nightly.20261003.2632",
            -1,
        ),
        ("001", "1", 0),
        ("1.5b", "1.5", -1),
        ("1.5", "1.5b", 1),
        ("a", "b", -1),
        ("1", "a", 1),
        ("1.0+1", "1.0.1", 0),
        ("2:1.0", "1:3.0", 1),
        ("1.0-1.1", "1.0-1", 1),
        ("5.2.0-1.1", "5.2.1-1.1", -1),
        ("3:26.2.3-1", "3:26.2.4-1", -1),
        ("0.56.2-3.1", "0.56.2-4.1", -1),
    ];

    #[test]
    fn matches_pacman_vercmp() {
        for &(a, b, want) in CASES {
            let want = want.cmp(&0);
            assert_eq!(vercmp(a, b), want, "vercmp({a}, {b})");
            assert_eq!(vercmp(b, a), want.reverse(), "vercmp({b}, {a})");
        }
    }

    #[test]
    fn pkgver_strips_epoch_and_rel() {
        assert_eq!(pkgver("3:26.2.3-1"), "26.2.3");
        assert_eq!(pkgver("5.2.0-1.1"), "5.2.0");
        assert_eq!(
            pkgver("5.0.0.r1191.g39a4a335c-25"),
            "5.0.0.r1191.g39a4a335c"
        );
        assert_eq!(pkgver("1.0"), "1.0");
    }
}
