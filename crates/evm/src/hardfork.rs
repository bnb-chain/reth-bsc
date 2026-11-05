#![allow(unused)]
use reth_chainspec::ForkCondition;
use reth_ethereum_forks::{hardfork, ChainHardforks, EthereumHardfork, Hardfork};
use revm_primitives::hardfork::SpecId;

hardfork!(
    /// The name of a bsc hardfork.
    ///
    /// When building a list of hardforks for a chain, it's still expected to mix with
    /// [`EthereumHardfork`].
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    BscHardfork {
        /// Initial hardfork of BSC.
        Frontier,
        /// BSC `Ramanujan` hardfork
        Ramanujan,
        /// BSC `Niels` hardfork
        Niels,
        /// BSC `MirrorSync` hardfork
        MirrorSync,
        /// BSC `Bruno` hardfork
        Bruno,
        /// BSC `Euler` hardfork
        Euler,
        /// BSC `Nano` hardfork
        Nano,
        /// BSC `Moran` hardfork
        Moran,
        /// BSC `Gibbs` hardfork
        Gibbs,
        /// BSC `Planck` hardfork
        Planck,
        /// BSC `Luban` hardfork
        Luban,
        /// BSC `Plato` hardfork
        Plato,
        /// BSC `Hertz` hardfork
        Hertz,
        /// BSC `HertzFix` hardfork
        HertzFix,
        /// BSC `Kepler` hardfork
        Kepler,
        /// BSC `Feynman` hardfork
        Feynman,
        /// BSC `FeynmanFix` hardfork
        FeynmanFix,
        /// BSC `Cancun` hardfork
        Cancun,
        /// BSC `Haber` hardfork
        Haber,
        /// BSC `HaberFix` hardfork
        HaberFix,
        /// BSC `Bohr` hardfork
        Bohr,
        /// BSC `Tycho` hardfork - June 2024, added blob transaction support
        Tycho,
        /// BSC `Pascal` hardfork - March 2025, added smart contract wallets
        Pascal,
        /// BSC `Lorentz` hardfork
        Lorentz,
        /// BSC `Maxwell` hardfork
        Maxwell,
        /// BSC `Fermi` hardfork
        #[default]
        Fermi,
        /// BSC `Osaka` hardfork
        Osaka,
        /// BSC `Mendel` hardfork
        Mendel,
        /// BSC `Pasteur` hardfork - sequenced immediately after Mendel
        Pasteur,
        /// BSC `Jenner` hardfork - sequenced immediately after Pasteur
        Jenner,
    }
);

impl BscHardfork {
    /// Bsc mainnet list of hardforks.
    pub fn bsc_mainnet() -> ChainHardforks {
        ChainHardforks::new(vec![
            (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
            (Self::Ramanujan.boxed(), ForkCondition::Block(0)),
            (Self::Niels.boxed(), ForkCondition::Block(0)),
            (Self::MirrorSync.boxed(), ForkCondition::Block(5184000)),
            (Self::Bruno.boxed(), ForkCondition::Block(13082000)),
            (Self::Euler.boxed(), ForkCondition::Block(18907621)),
            (Self::Nano.boxed(), ForkCondition::Block(21962149)),
            (Self::Moran.boxed(), ForkCondition::Block(22107423)),
            (Self::Gibbs.boxed(), ForkCondition::Block(23846001)),
            (Self::Planck.boxed(), ForkCondition::Block(27281024)),
            (Self::Luban.boxed(), ForkCondition::Block(29020050)),
            (Self::Plato.boxed(), ForkCondition::Block(30720096)),
            (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(31302048)),
            (EthereumHardfork::London.boxed(), ForkCondition::Block(31302048)),
            (Self::Hertz.boxed(), ForkCondition::Block(31302048)),
            (Self::HertzFix.boxed(), ForkCondition::Block(34140700)),
            (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(1705996800)), /* 2024-01-23 08:00:00 AM UTC */
            (Self::Kepler.boxed(), ForkCondition::Timestamp(1705996800)), /* 2024-01-23 08:00:00
                                                                           * AM UTC */
            (Self::Feynman.boxed(), ForkCondition::Timestamp(1713419340)), /* 2024-04-18
                                                                            * 05:49:00 AM UTC */
            (Self::FeynmanFix.boxed(), ForkCondition::Timestamp(1713419340)), /* 2024-04-18 05:49:00 AM UTC */
            (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(1718863500)), /* 2024-06-20 06:05:00 AM UTC */
            (Self::Cancun.boxed(), ForkCondition::Timestamp(1718863500)), /* 2024-06-20 06:05:00
                                                                           * AM UTC */
            (Self::Haber.boxed(), ForkCondition::Timestamp(1718863500)), /* 2024-06-20 06:05:00
                                                                          * AM UTC */
            (Self::Tycho.boxed(), ForkCondition::Timestamp(1718863500)), /* 2024-06-20 06:05:00
                                                                          * AM UTC - Tycho
                                                                          * hardfork with blob
                                                                          * transactions */
            (Self::HaberFix.boxed(), ForkCondition::Timestamp(1727316120)), /* 2024-09-26 02:02:00 AM UTC */
            (Self::Bohr.boxed(), ForkCondition::Timestamp(1727317200)),     /* 2024-09-26
                                                                             * 02:20:00
                                                                             * AM UTC */
            (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(1742436600)), /* 2025-03-20 02:10:00 AM UTC */
            (Self::Pascal.boxed(), ForkCondition::Timestamp(1742436600)), /* 2025-03-20 02:10:00
                                                                           * AM UTC - deployed
                                                                           * with Prague */
            (Self::Lorentz.boxed(), ForkCondition::Timestamp(1745903100)), /* 2025-04-29
                                                                            * 05:05:00 AM UTC */
            (Self::Maxwell.boxed(), ForkCondition::Timestamp(1751250600)), /* 2025-06-30
                                                                            * 02:30:00 AM UTC */
            (Self::Fermi.boxed(), ForkCondition::Timestamp(1768357800)), /* 2026-01-14 02:30:00
                                                                          * AM UTC */
            (Self::Osaka.boxed(), ForkCondition::Timestamp(1777343400)),
            (Self::Mendel.boxed(), ForkCondition::Timestamp(1777343400)),
            (Self::Pasteur.boxed(), ForkCondition::Timestamp(1787625000)), /* 2026-08-25
                                                                            * 02:30:00 AM UTC */
        ])
    }

    /// Bsc testnet list of hardforks.
    pub fn bsc_testnet() -> ChainHardforks {
        ChainHardforks::new(vec![
            (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
            (Self::Ramanujan.boxed(), ForkCondition::Block(1010000)),
            (Self::Niels.boxed(), ForkCondition::Block(1014369)),
            (Self::MirrorSync.boxed(), ForkCondition::Block(5582500)),
            (Self::Bruno.boxed(), ForkCondition::Block(13837000)),
            (Self::Euler.boxed(), ForkCondition::Block(19203503)),
            (Self::Gibbs.boxed(), ForkCondition::Block(22800220)),
            (Self::Nano.boxed(), ForkCondition::Block(23482428)),
            (Self::Moran.boxed(), ForkCondition::Block(23603940)),
            (Self::Planck.boxed(), ForkCondition::Block(28196022)),
            (Self::Luban.boxed(), ForkCondition::Block(29295050)),
            (Self::Plato.boxed(), ForkCondition::Block(29861024)),
            (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(31103030)),
            (EthereumHardfork::London.boxed(), ForkCondition::Block(31103030)),
            (Self::Hertz.boxed(), ForkCondition::Block(31103030)),
            (Self::HertzFix.boxed(), ForkCondition::Block(35682300)),
            (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(1702972800)),
            (Self::Kepler.boxed(), ForkCondition::Timestamp(1702972800)),
            (Self::Feynman.boxed(), ForkCondition::Timestamp(1710136800)),
            (Self::FeynmanFix.boxed(), ForkCondition::Timestamp(1711342800)),
            (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(1713330442)),
            (Self::Cancun.boxed(), ForkCondition::Timestamp(1713330442)),
            (Self::Haber.boxed(), ForkCondition::Timestamp(1716962820)),
            (Self::HaberFix.boxed(), ForkCondition::Timestamp(1719986788)),
            (Self::Bohr.boxed(), ForkCondition::Timestamp(1724116996)),
            (Self::Tycho.boxed(), ForkCondition::Timestamp(1713330442)), /* 2024-04-17 05:07:22
                                                                          * AM UTC - Tycho
                                                                          * testnet */
            (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(1740452880)),
            (Self::Pascal.boxed(), ForkCondition::Timestamp(1740452880)),
            (Self::Lorentz.boxed(), ForkCondition::Timestamp(1744097580)),
            (Self::Maxwell.boxed(), ForkCondition::Timestamp(1748243100)),
            (Self::Fermi.boxed(), ForkCondition::Timestamp(1762741500)),
            (Self::Osaka.boxed(), ForkCondition::Timestamp(1774319400)),
            (Self::Mendel.boxed(), ForkCondition::Timestamp(1774319400)),
            (Self::Pasteur.boxed(), ForkCondition::Timestamp(1784601000)), /* 2026-07-21
                                                                            * 02:30:00 AM UTC */
            (Self::Jenner.boxed(), ForkCondition::Timestamp(1793241000)), /* 2026-10-29 02:30:00
                                                                           * AM UTC */
        ])
    }

    /// Bsc qanet list of hardforks.
    pub fn bsc_qanet() -> ChainHardforks {
        ChainHardforks::new(vec![
            (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
            (EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)),
            (Self::Ramanujan.boxed(), ForkCondition::Block(0)),
            (Self::Niels.boxed(), ForkCondition::Block(0)),
            (Self::MirrorSync.boxed(), ForkCondition::Block(1)),
            (Self::Bruno.boxed(), ForkCondition::Block(1)),
            (Self::Euler.boxed(), ForkCondition::Block(2)),
            (Self::Nano.boxed(), ForkCondition::Block(3)),
            (Self::Moran.boxed(), ForkCondition::Block(3)),
            (Self::Gibbs.boxed(), ForkCondition::Block(4)),
            (Self::Planck.boxed(), ForkCondition::Block(5)),
            (Self::Luban.boxed(), ForkCondition::Block(6)),
            (Self::Plato.boxed(), ForkCondition::Block(7)),
            (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(8)),
            (EthereumHardfork::London.boxed(), ForkCondition::Block(8)),
            (Self::Hertz.boxed(), ForkCondition::Block(8)),
            (Self::HertzFix.boxed(), ForkCondition::Block(8)),
            (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Kepler.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Feynman.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::FeynmanFix.boxed(), ForkCondition::Timestamp(1754967081)),
            (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Cancun.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Haber.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::HaberFix.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Bohr.boxed(), ForkCondition::Timestamp(1754967081)),
            (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Pascal.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Lorentz.boxed(), ForkCondition::Timestamp(1754967081)),
            (Self::Maxwell.boxed(), ForkCondition::Timestamp(1754967101)),
            (Self::Fermi.boxed(), ForkCondition::Timestamp(1761030900)),
            // Osaka and Mendel activated on qanet at 2026-08-05 02:30:00 AM UTC. Both are
            // geth-compatible: geth accepted blocks 21323714..21327713 with only Pasteur deferred.
            (Self::Osaka.boxed(), ForkCondition::Timestamp(1785897000)),
            (Self::Mendel.boxed(), ForkCondition::Timestamp(1785897000)),
            // Pasteur: 2026-08-05 09:00:00 AM UTC. The first attempt (at 1785897000, block
            // 21323714) split the cluster because the system-contract upgrade wrote the new
            // StakeHub/Governor code to plain state without committing the changed codeHash to the
            // trie, so the block's state root described the un-upgraded state and geth rejected it
            // (`invalid merkle root`). Fixed by reporting those writes to the state hook; this
            // re-arms the transition to verify the fix on qanet.
            // Must stay equal to geth's `--override.pasteur` on this cluster.
            (Self::Pasteur.boxed(), ForkCondition::Timestamp(1785920400)),
            // Jenner: 2026-10-05 05:00:00 UTC (13:00:00 UTC+8).
            (Self::Jenner.boxed(), ForkCondition::Timestamp(1791176400)),
        ])
    }

    pub fn bsc_local() -> ChainHardforks {
        ChainHardforks::new(vec![
            (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
            (Self::Bohr.boxed(), ForkCondition::Block(0)),
        ])
    }
}

impl From<BscHardfork> for SpecId {
    fn from(spec: BscHardfork) -> Self {
        match spec {
            BscHardfork::Frontier |
            BscHardfork::Ramanujan |
            BscHardfork::Niels |
            BscHardfork::MirrorSync |
            BscHardfork::Bruno |
            BscHardfork::Euler |
            BscHardfork::Gibbs |
            BscHardfork::Nano |
            BscHardfork::Moran |
            BscHardfork::Planck |
            BscHardfork::Luban |
            BscHardfork::Plato => SpecId::ISTANBUL,
            BscHardfork::Hertz | BscHardfork::HertzFix => SpecId::LONDON,
            BscHardfork::Kepler | BscHardfork::Feynman | BscHardfork::FeynmanFix => {
                SpecId::SHANGHAI
            }
            BscHardfork::Cancun |
            BscHardfork::Haber |
            BscHardfork::HaberFix |
            BscHardfork::Bohr |
            BscHardfork::Tycho => SpecId::CANCUN,
            BscHardfork::Pascal |
            BscHardfork::Lorentz |
            BscHardfork::Maxwell |
            BscHardfork::Fermi => SpecId::PRAGUE,
            BscHardfork::Osaka |
            BscHardfork::Mendel |
            BscHardfork::Pasteur |
            BscHardfork::Jenner => SpecId::OSAKA,
        }
    }
}

impl From<BscHardfork> for evm2::SpecId {
    fn from(spec: BscHardfork) -> Self {
        match spec {
            BscHardfork::Frontier |
            BscHardfork::Ramanujan |
            BscHardfork::Niels |
            BscHardfork::MirrorSync |
            BscHardfork::Bruno |
            BscHardfork::Euler |
            BscHardfork::Gibbs |
            BscHardfork::Nano |
            BscHardfork::Moran |
            BscHardfork::Planck |
            BscHardfork::Luban |
            BscHardfork::Plato => evm2::SpecId::ISTANBUL,
            BscHardfork::Hertz | BscHardfork::HertzFix => evm2::SpecId::LONDON,
            BscHardfork::Kepler | BscHardfork::Feynman | BscHardfork::FeynmanFix => {
                evm2::SpecId::SHANGHAI
            }
            BscHardfork::Cancun |
            BscHardfork::Haber |
            BscHardfork::HaberFix |
            BscHardfork::Bohr |
            BscHardfork::Tycho => evm2::SpecId::CANCUN,
            BscHardfork::Pascal |
            BscHardfork::Lorentz |
            BscHardfork::Maxwell |
            BscHardfork::Fermi => evm2::SpecId::PRAGUE,
            BscHardfork::Osaka |
            BscHardfork::Mendel |
            BscHardfork::Pasteur |
            BscHardfork::Jenner => evm2::SpecId::OSAKA,
        }
    }
}
