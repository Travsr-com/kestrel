//! Scenarios 8 and 9: MicroUsd and RunId.

use kestrel_types::{MicroUsd, RunId};
use proptest::prelude::*;

#[test]
fn s8_checked_add_overflow_is_none() {
    assert_eq!(MicroUsd::MAX.checked_add(1), None);
    assert_eq!(MicroUsd(1).checked_add(2), Some(MicroUsd(3)));
    assert_eq!(MicroUsd(i64::MAX - 1).checked_add(1), Some(MicroUsd::MAX));
}

#[test]
fn s8_checked_sub_underflow_is_none() {
    assert_eq!(MicroUsd(i64::MIN).checked_sub(1), None);
    assert_eq!(MicroUsd(5).checked_sub(2), Some(MicroUsd(3)));
}

#[test]
fn s8_saturating_sub_floors_at_zero() {
    assert_eq!(MicroUsd(3).saturating_sub(MicroUsd(5)), MicroUsd(0));
    assert_eq!(MicroUsd(5).saturating_sub(MicroUsd(3)), MicroUsd(2));
    assert_eq!(MicroUsd(0).saturating_sub(MicroUsd::MAX), MicroUsd(0));
}

#[test]
fn s8_display_format() {
    assert_eq!(MicroUsd(1_234_567).to_string(), "$1.234567");
    assert_eq!(MicroUsd(2_000_000).to_string(), "$2.000000");
    assert_eq!(MicroUsd(1).to_string(), "$0.000001");
    assert_eq!(MicroUsd(0).to_string(), "$0.000000");
}

proptest! {
    #[test]
    fn s8_saturating_sub_never_negative(a in 0i64..=i64::MAX, b in 0i64..=i64::MAX) {
        let r = MicroUsd(a).saturating_sub(MicroUsd(b));
        prop_assert!(r.0 >= 0);
        prop_assert_eq!(r.0, (a - b).max(0));
    }

    #[test]
    fn s8_checked_add_matches_i64(a in any::<i64>(), b in any::<i64>()) {
        prop_assert_eq!(MicroUsd(a).checked_add(b), a.checked_add(b).map(MicroUsd));
    }

    #[test]
    fn s8_display_round_trips_digits(v in 0i64..=i64::MAX) {
        let s = MicroUsd(v).to_string();
        let digits = s.strip_prefix('$').expect("dollar prefix");
        let (whole, frac) = digits.split_once('.').expect("decimal point");
        prop_assert_eq!(frac.len(), 6);
        let back = whole.parse::<i64>().expect("whole") * 1_000_000 + frac.parse::<i64>().expect("frac");
        prop_assert_eq!(back, v);
    }
}

#[test]
fn s9_run_id_display_parse_round_trip() {
    let id = RunId::new();
    let text = id.to_string();
    assert_eq!(text.parse::<RunId>().expect("parse"), id);
    assert_eq!(text.len(), 36);
}

#[test]
fn s9_run_id_rejects_garbage() {
    assert!("not-a-uuid".parse::<RunId>().is_err());
    assert!("".parse::<RunId>().is_err());
}

#[test]
fn s9_run_ids_sort_in_creation_order() {
    let ids: Vec<RunId> = (0..1000).map(|_| RunId::new()).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids must be strictly increasing"
    );
}

#[test]
fn s9_run_id_is_uuid_v7() {
    let id = RunId::new().to_string();
    assert_eq!(id.as_bytes()[14], b'7', "{id}");
}

#[test]
fn parse_decimal_is_exact_and_rejects_everything_else() {
    assert_eq!(MicroUsd::parse_decimal("5"), Some(MicroUsd(5_000_000)));
    assert_eq!(MicroUsd::parse_decimal("2.50"), Some(MicroUsd(2_500_000)));
    assert_eq!(MicroUsd::parse_decimal("0.000001"), Some(MicroUsd(1)));
    for bad in [
        "",
        "-1",
        "abc",
        "1e3",
        "0.0000001",
        "+1",
        "99999999999999999999",
    ] {
        assert_eq!(MicroUsd::parse_decimal(bad), None, "{bad:?}");
    }
}
