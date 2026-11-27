//! The base fee rule a chain's header carries, and the fee growth it allows.

use alloy::primitives::hex;
use relayer::config::Eip1559;

fn extra(version: u8, denominator: u32, elasticity: u32, min: &[u8]) -> Vec<u8> {
    let (d, e) = (denominator.to_be_bytes(), elasticity.to_be_bytes());
    [&[version][..], &d, &e, min].concat()
}

#[test]
fn growth_follows_the_chains_rule() {
    // Base mainnet's header, Jovian: 0.4% a block above 5,000,000 wei.
    let base = hex!("01000003e80000000500000000004c4b40");
    let base = Eip1559::from_extra_data(&base).unwrap();
    assert_eq!(base.min_base_fee, 5_000_000);
    assert_eq!(base.grow(1_000_000, 0), 5_000_000);
    assert_eq!(base.grow(1_000_000, 2), 5_040_080);
    // Base Sepolia's header: 1.4% a block, with the same floor.
    let sepolia = hex!("01000003e80000000f00000000004c4b40");
    let sepolia = Eip1559::from_extra_data(&sepolia).unwrap();
    assert_eq!(sepolia.grow(6_000_000, 1), 6_084_000);
    // Holocene's version 0 has no floor; each block rounds up.
    let holocene = Eip1559::from_extra_data(&extra(0, 1000, 15, &[])).unwrap();
    assert_eq!(holocene.grow(999, 1), 1013);
    // L1: 12.5% a block.
    assert_eq!(Eip1559::L1.grow(1000, 2), 1266);
    // M over 5 blocks: 1.125^5 = 1.802 on L1, 1.004^5 = 1.020 on Base, its floor ignored.
    assert_eq!(Eip1559::L1.growth_percent(5), 181);
    assert_eq!(base.growth_percent(5), 103);
    // Unrecognised: a version's wrong length, an unknown version, a zero parameter.
    let min = 1u64.to_be_bytes();
    for bad in [
        extra(0, 1000, 15, &min),
        extra(1, 1000, 5, &[]),
        extra(2, 1000, 5, &min),
        extra(0, 0, 15, &[]),
        vec![],
    ] {
        assert_eq!(Eip1559::from_extra_data(&bad), None, "{bad:?}");
    }
}
