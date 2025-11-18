//! Hard forks of bsc protocol.
#![allow(unused)]
use bsc::BscHardfork;
use reth_chainspec::{EthereumHardforks, ForkCondition};

pub mod bsc;

/// Extends [`EthereumHardforks`] with bsc helper methods.
pub trait BscHardforks: EthereumHardforks {
    /// Retrieves [`ForkCondition`] by an [`BscHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn bsc_fork_activation(&self, fork: BscHardfork) -> ForkCondition;

    /// Convenience method to check if [`BscHardfork::Ramanujan`] is firstly active at a given
    /// block.
    fn is_ramanujan_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Ramanujan).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Ramanujan`] is active at a given block.
    fn is_ramanujan_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Ramanujan).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Niels`] is firstly active at a given block.
    fn is_niels_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Niels).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Niels`] is active at a given block.
    fn is_niels_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Niels).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::MirrorSync`] is firstly active at a given block.
    fn is_mirror_sync_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::MirrorSync).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::MirrorSync`] is active at a given block.
    fn is_mirror_sync_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::MirrorSync).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Bruno`] is firstly active at a given block.
    fn is_bruno_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Bruno).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Bruno`] is active at a given block.
    fn is_bruno_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Bruno).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Euler`] is firstly active at a given block.
    fn is_euler_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Euler).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Euler`] is active at a given block.
    fn is_euler_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Euler).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Nano`] is firstly active at a given block.
    fn is_nano_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Nano).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Nano`] is active at a given block.
    fn is_nano_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Nano).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Moran`] is firstly active at a given block.
    fn is_moran_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Moran).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Moran`] is active at a given block.
    fn is_moran_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Moran).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Gibbs`] is firstly active at a given block.
    fn is_gibbs_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Gibbs).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Gibbs`] is active at a given block.
    fn is_gibbs_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Gibbs).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Planck`] is firstly active at a given block.
    fn is_planck_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Planck).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Planck`] is active at a given block.
    fn is_planck_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Planck).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Luban`] is firstly active at a given block.
    fn is_luban_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Luban).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Luban`] is active at a given block.
    fn is_luban_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Luban).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Plato`] is firstly active at a given block.
    fn is_plato_transition_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Plato).transitions_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Plato`] is active at a given block.
    fn is_plato_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Plato).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Hertz`] is active at a given block.
    fn is_hertz_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::Hertz).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::HertzFix`] is active at a given block.
    fn is_hertz_fix_active_at_block(&self, block_number: u64) -> bool {
        self.bsc_fork_activation(BscHardfork::HertzFix).active_at_block(block_number)
    }

    /// Convenience method to check if [`BscHardfork::Kepler`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_kepler_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_kepler_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_kepler_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Kepler`] is active at a given timestamp.
    fn is_kepler_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Kepler).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Feynman`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_feynman_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_feynman_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_feynman_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Feynman`] is active at a given timestamp.
    fn is_feynman_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Feynman).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::FeynmanFix`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_feynman_fix_transition_at_timestamp(
        &self,
        block_number: u64,
        timestamp: u64,
        parent_timestamp: u64,
    ) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_feynman_fix_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_feynman_fix_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::FeynmanFix`] is active at a given timestamp.
    fn is_feynman_fix_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::FeynmanFix).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Haber`] is firstly active at a given timestamp
    /// and parent timestamp.
    fn is_haber_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_haber_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_haber_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Haber`] is active at a given timestamp.
    fn is_haber_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Haber).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Tycho`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_tycho_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_tycho_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_tycho_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Tycho`] is active at a given timestamp.
    fn is_tycho_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Tycho).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::HaberFix`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_haber_fix_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_haber_fix_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_haber_fix_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::HaberFix`] is active at a given timestamp.
    fn is_haber_fix_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::HaberFix).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Cancun`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_cancun_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !BscHardforks::is_cancun_active_at_timestamp(self, parent_number, parent_timestamp)
            && BscHardforks::is_cancun_active_at_timestamp(self, block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Cancun`] is active at a given timestamp.
    fn is_cancun_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Cancun).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Bohr`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_bohr_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !BscHardforks::is_bohr_active_at_timestamp(self, parent_number, parent_timestamp)
            && BscHardforks::is_bohr_active_at_timestamp(self, block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Bohr`] is active at a given timestamp.
    fn is_bohr_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Bohr).active_at_timestamp(timestamp)
    }
    /// Convenience method to check if [`EthereumHardfork::Prague`] is active at a given block
    /// and timestamp.
    fn is_prague_active_at_block_and_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number)
            && reth_chainspec::EthereumHardforks::is_prague_active_at_timestamp(self, timestamp)
    }

    /// Convenience method to check if [`EthereumHardfork::Prague`] is firstly active at a given
    /// block/timestamp and its parent block/timestamp.
    fn is_prague_transition_at_block_and_timestamp(
        &self,
        block_number: u64,
        timestamp: u64,
        parent_timestamp: u64,
    ) -> bool {
        let parent_number = block_number.saturating_sub(1);
        self.is_prague_active_at_block_and_timestamp(block_number, timestamp)
            && !self.is_prague_active_at_block_and_timestamp(parent_number, parent_timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Pascal`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_pascal_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_pascal_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_pascal_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Pascal`] is active at a given timestamp.
    fn is_pascal_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Pascal).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Lorentz`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_lorentz_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_lorentz_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_lorentz_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Lorentz`] is active at a given timestamp.
    fn is_lorentz_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Lorentz).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Maxwell`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_maxwell_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_maxwell_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_maxwell_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Maxwell`] is active at a given timestamp.
    fn is_maxwell_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Maxwell).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Fermi`] is firstly active at a given
    /// timestamp and parent timestamp.
    fn is_fermi_transition_at_timestamp(&self, block_number: u64, timestamp: u64, parent_timestamp: u64) -> bool {
        let parent_number = block_number.saturating_sub(1);
        !self.is_fermi_active_at_timestamp(parent_number, parent_timestamp)
            && self.is_fermi_active_at_timestamp(block_number, timestamp)
    }

    /// Convenience method to check if [`BscHardfork::Fermi`] is active at a given timestamp.
    fn is_fermi_active_at_timestamp(&self, block_number: u64, timestamp: u64) -> bool {
        self.is_london_active_at_block(block_number) &&
        self.bsc_fork_activation(BscHardfork::Fermi).active_at_timestamp(timestamp)
    }
}
