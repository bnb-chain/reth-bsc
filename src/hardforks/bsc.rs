pub use reth_bsc_evm::hardfork::BscHardfork;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainspec::{bsc::bsc_mainnet, bsc_chapel::bsc_testnet};
    use reth_chainspec::ForkCondition;
    use revm::primitives::hardfork::SpecId;

    #[test]
    fn test_pasteur_is_sequenced_after_mendel() {
        // Derived ordering must place Pasteur as the highest fork so that
        // `spec >= BscHardfork::Pasteur` is the first/highest dispatcher check.
        assert!(BscHardfork::Pasteur > BscHardfork::Mendel);
        assert!(BscHardfork::Pasteur > BscHardfork::Osaka);
        // Pasteur is a BSC-only precompile/system-contract fork with no new EVM spec.
        assert_eq!(SpecId::from(BscHardfork::Pasteur), SpecId::OSAKA);
    }

    #[test]
    fn test_pasteur_scheduled_on_testnet() {
        // Testnet has an announced Pasteur activation: 2026-07-21 02:30:00 AM UTC.
        assert_eq!(
            BscHardfork::bsc_testnet().fork(BscHardfork::Pasteur),
            ForkCondition::Timestamp(1784601000),
            "Pasteur should activate on testnet at its announced timestamp"
        );
    }

    #[test]
    fn test_pasteur_scheduled_on_mainnet() {
        // Mainnet has an announced Pasteur activation: 2026-08-25 02:30:00 AM UTC.
        assert_eq!(
            BscHardfork::bsc_mainnet().fork(BscHardfork::Pasteur),
            ForkCondition::Timestamp(1787625000),
            "Pasteur should activate on mainnet at its announced timestamp"
        );
    }

    #[test]
    fn test_qanet_fork_schedule_is_ordered() {
        let qanet = BscHardfork::bsc_qanet();
        const OSAKA_MENDEL: u64 = 1785897000; // 2026-08-05 02:30:00 UTC
        const PASTEUR: u64 = 1785920400; // 2026-08-05 09:00:00 UTC
        const JENNER: u64 = 1791176400; // 2026-10-05 05:00:00 UTC (13:00:00 UTC+8)

        // Osaka and Mendel activated together at 2026-08-05 02:30:00 AM UTC.
        for fork in [BscHardfork::Osaka, BscHardfork::Mendel] {
            assert_eq!(
                qanet.fork(fork),
                ForkCondition::Timestamp(OSAKA_MENDEL),
                "{fork:?} should be active on qanet from the 2026-08-05 02:30 UTC activation"
            );
        }

        // Pasteur re-armed after the state-hook fix. It must stay in sync with geth's
        // `--override.pasteur` on the QA cluster, and must not precede Osaka/Mendel.
        assert_eq!(
            qanet.fork(BscHardfork::Pasteur),
            ForkCondition::Timestamp(PASTEUR),
            "Pasteur should activate on qanet at its re-armed timestamp"
        );
        // Ordering is compile-time enforced: Pasteur must never precede Osaka/Mendel.
        const _: () = assert!(PASTEUR > OSAKA_MENDEL);
        assert!(
            qanet.fork(BscHardfork::Mendel).active_at_timestamp(PASTEUR),
            "Mendel must already be active when Pasteur activates"
        );

        let jenner = qanet.fork(BscHardfork::Jenner);
        assert_eq!(jenner, ForkCondition::Timestamp(JENNER));
        const _: () = assert!(JENNER > PASTEUR);
        assert!(
            qanet.fork(BscHardfork::Pasteur).active_at_timestamp(JENNER),
            "Pasteur must already be active when Jenner activates"
        );
        assert!(!jenner.active_at_timestamp(JENNER - 1));
        assert!(jenner.active_at_timestamp(JENNER));
        assert!(jenner.active_at_timestamp(JENNER + 1));

        // Fermi precedes the whole group.
        assert!(
            qanet.fork(BscHardfork::Fermi).active_at_timestamp(1785897000),
            "Fermi must already be active when Osaka/Mendel activate"
        );
    }

    #[test]
    fn test_pasteur_resolves_at_its_timestamp() {
        use crate::node::evm::config::revm_spec_by_timestamp_and_block_number;

        // Give Pasteur a concrete activation just after Mendel (1777343400 on mainnet).
        let pasteur_time = 1_777_343_400 + 1_000;
        let mut cs = bsc_mainnet();
        cs.hardforks.insert(BscHardfork::Pasteur, ForkCondition::Timestamp(pasteur_time));
        let spec = crate::chainspec::BscChainSpec::from(cs);

        let block = 50_000_000; // well past London activation (31302048)

        // Just before Pasteur, the latest fork is still Mendel.
        assert_eq!(
            revm_spec_by_timestamp_and_block_number(spec.clone(), pasteur_time - 1, block),
            BscHardfork::Mendel
        );
        // At/after Pasteur's timestamp, it resolves to Pasteur.
        assert_eq!(
            revm_spec_by_timestamp_and_block_number(spec.clone(), pasteur_time, block),
            BscHardfork::Pasteur
        );
    }

    #[test]
    fn test_jenner_is_sequenced_after_pasteur() {
        // Derived ordering must place Jenner (BEP-706) right after Pasteur so that
        // `spec >= BscHardfork::Jenner` is the highest dispatcher check
        // (mirrors go-bsc: Jenner sits between Pasteur and the far-future upstream forks).
        assert!(BscHardfork::Jenner > BscHardfork::Pasteur);
        assert!(BscHardfork::Jenner > BscHardfork::Mendel);
        assert!(BscHardfork::Jenner > BscHardfork::Osaka);
        // Jenner adds a BSC-only precompile; it introduces no new EVM rule set.
        assert_eq!(SpecId::from(BscHardfork::Jenner), SpecId::OSAKA);
    }

    #[test]
    fn test_jenner_scheduled_on_testnet() {
        // Chapel JennerTime matches go-bsc: 2026-10-29 02:30:00 AM UTC (1793241000).
        assert_eq!(
            BscHardfork::bsc_testnet().fork(BscHardfork::Jenner),
            ForkCondition::Timestamp(1793241000),
            "Jenner should activate on testnet at its announced timestamp"
        );
    }

    #[test]
    fn test_jenner_not_scheduled_on_other_builtin_chains() {
        // Mirrors go-bsc `JennerTime: nil` on BSCChainConfig: mainnet activation is not
        // scheduled in this release. Local stays unscheduled.
        for (name, forks) in
            [("mainnet", BscHardfork::bsc_mainnet()), ("local", BscHardfork::bsc_local())]
        {
            assert_eq!(
                forks.fork(BscHardfork::Jenner),
                ForkCondition::Never,
                "Jenner must stay unscheduled on {name} until its activation is announced"
            );
        }
    }

    #[test]
    fn test_is_jenner_activation_boundary() {
        use crate::hardforks::BscHardforks;

        // Mirrors go-bsc TestIsJenner: exactly at JennerTime -> active; one second
        // before -> inactive; one second after -> active.
        let jenner_time = 1_790_000_000u64;
        let mut cs = bsc_mainnet();
        cs.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(jenner_time));
        let spec = crate::chainspec::BscChainSpec::from(cs);

        let block = 50_000_000; // well past London activation (31302048)
        assert!(!spec.is_jenner_active_at_timestamp(block, jenner_time - 1));
        assert!(spec.is_jenner_active_at_timestamp(block, jenner_time));
        assert!(spec.is_jenner_active_at_timestamp(block, jenner_time + 1));

        // The convention shared by every timestamp fork helper: inert before London.
        let pre_london_block = 1_000;
        assert!(!spec.is_jenner_active_at_timestamp(pre_london_block, jenner_time));

        // Transition helper: first block whose timestamp crosses the boundary.
        assert!(spec.is_jenner_transition_at_timestamp(block, jenner_time, jenner_time - 1));
        assert!(!spec.is_jenner_transition_at_timestamp(block, jenner_time + 1, jenner_time));
        assert!(!spec.is_jenner_transition_at_timestamp(block, jenner_time - 1, jenner_time - 2));
    }

    #[test]
    fn test_jenner_resolves_at_its_timestamp() {
        use crate::node::evm::config::revm_spec_by_timestamp_and_block_number;

        // Give Jenner a concrete activation just after Pasteur (1787625000 on mainnet).
        let jenner_time = 1_787_625_000 + 1_000;
        let mut cs = bsc_mainnet();
        cs.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(jenner_time));
        let spec = crate::chainspec::BscChainSpec::from(cs);

        let block = 50_000_000; // well past London activation (31302048)

        // Just before Jenner, the latest fork is still Pasteur.
        assert_eq!(
            revm_spec_by_timestamp_and_block_number(spec.clone(), jenner_time - 1, block),
            BscHardfork::Pasteur
        );
        // At/after Jenner's timestamp, it resolves to Jenner.
        assert_eq!(
            revm_spec_by_timestamp_and_block_number(spec.clone(), jenner_time, block),
            BscHardfork::Jenner
        );
        // Cross-fork regression (mirrors go-bsc TestRulesJennerRegression): with Jenner
        // unscheduled, resolution at the same instants is unchanged.
        let plain = crate::chainspec::BscChainSpec::from(bsc_mainnet());
        assert_eq!(
            revm_spec_by_timestamp_and_block_number(plain.clone(), jenner_time, block),
            BscHardfork::Pasteur
        );
        assert_eq!(
            revm_spec_by_timestamp_and_block_number(plain, jenner_time - 1, block),
            BscHardfork::Pasteur
        );
    }

    #[test]
    fn test_hardfork_activation_order_differences() {
        // Test the critical difference between mainnet and testnet activation orders
        // This demonstrates why the order in revm_spec_by_timestamp_and_block_number matters

        // Mainnet activation blocks (from the code):
        // Euler: 18907621, Nano: 21962149, Moran: 22107423, Gibbs: 23846001
        // Order: Gibbs -> Moran -> Nano -> Euler (newest to oldest)

        // Testnet activation blocks (from the code):
        // Euler: 19203503, Gibbs: 22800220, Nano: 23482428, Moran: 23603940
        // Order: Moran -> Nano -> Gibbs -> Euler (newest to oldest)

        // Test mainnet chain spec
        let mainnet_spec = crate::chainspec::BscChainSpec::from(bsc_mainnet());

        // Test blocks around the critical transition points
        // Block 23846000: Should be Moran (before Gibbs activation)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                mainnet_spec.clone(),
                1700000000, // Some timestamp
                23846000
            ),
            BscHardfork::Moran
        );

        // Block 23846001: Should be Gibbs (Gibbs activation block)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                mainnet_spec.clone(),
                1700000000, // Some timestamp
                23846001
            ),
            BscHardfork::Gibbs
        );

        // Block 22107422: Should be Nano (before Moran activation)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                mainnet_spec.clone(),
                1700000000, // Some timestamp
                22107422
            ),
            BscHardfork::Nano
        );

        // Block 22107423: Should be Moran (Moran activation block)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                mainnet_spec.clone(),
                1700000000, // Some timestamp
                22107423
            ),
            BscHardfork::Moran
        );

        // Test testnet chain spec
        let testnet_spec = crate::chainspec::BscChainSpec::from(bsc_testnet());

        // Test blocks around the critical transition points for testnet
        // Block 23603939: Should be Nano (before Moran activation)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                testnet_spec.clone(),
                1700000000, // Some timestamp
                23603939
            ),
            BscHardfork::Nano
        );

        // Block 23603940: Should be Moran (Moran activation block)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                testnet_spec.clone(),
                1700000000, // Some timestamp
                23603940
            ),
            BscHardfork::Moran
        );

        // Block 23482427: Should be Gibbs (before Nano activation)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                testnet_spec.clone(),
                1700000000, // Some timestamp
                23482427
            ),
            BscHardfork::Gibbs
        );

        // Block 23482428: Should be Nano (Nano activation block)
        assert_eq!(
            crate::node::evm::config::revm_spec_by_timestamp_and_block_number(
                testnet_spec.clone(),
                1700000000, // Some timestamp
                23482428
            ),
            BscHardfork::Nano
        );
    }
}
