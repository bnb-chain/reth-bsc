//! Native accesses for eth_createAccessList. Recording is scoped to synchronous
//! inspection and independent of the journal, so reverted calls remain visible.

use alloy_eips::eip2930::{AccessList, AccessListItem};
use alloy_primitives::{Address, B256, U256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
};

#[derive(Default)]
pub(crate) struct NativeAccesses(BTreeMap<Address, BTreeSet<B256>>);

thread_local! {
    static ACCESSES: RefCell<Option<NativeAccesses>> = const { RefCell::new(None) };
}

struct CaptureGuard(Option<NativeAccesses>);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        ACCESSES.with(|slot| slot.replace(self.0.take()));
    }
}

pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, NativeAccesses) {
    let guard = CaptureGuard(ACCESSES.with(|slot| slot.replace(Some(NativeAccesses::default()))));
    let result = f();
    let accesses = ACCESSES.with(|slot| slot.take().expect("active native access capture"));
    drop(guard);
    (result, accesses)
}

pub(super) fn storage(address: Address, key: U256) {
    ACCESSES.with(|slot| {
        if let Some(accesses) = slot.borrow_mut().as_mut() {
            accesses.0.entry(address).or_default().insert(B256::from(key));
        }
    });
}

pub(super) fn account(address: Address) {
    ACCESSES.with(|slot| {
        if let Some(accesses) = slot.borrow_mut().as_mut() {
            accesses.0.entry(address).or_default();
        }
    });
}

impl NativeAccesses {
    pub(crate) fn merge(self, list: AccessList, excluded: impl Fn(&Address) -> bool) -> AccessList {
        let mut merged = BTreeMap::<Address, BTreeSet<B256>>::new();
        for item in list.0 {
            merged.entry(item.address).or_default().extend(item.storage_keys);
        }
        for (address, keys) in self.0 {
            // Warm accounts can be omitted, but their storage keys must still be included.
            if !keys.is_empty() || !excluded(&address) {
                merged.entry(address).or_default().extend(keys);
            }
        }
        AccessList(
            merged
                .into_iter()
                .map(|(address, keys)| AccessListItem {
                    address,
                    storage_keys: keys.into_iter().collect(),
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_restore_after_panic_and_do_not_leak_between_requests() {
        let address = Address::repeat_byte(1);
        let (_, outer) = capture(|| {
            storage(address, U256::from(1));
            assert!(std::panic::catch_unwind(|| capture(|| {
                storage(address, U256::from(2));
                panic!("cancelled inspection");
            }))
            .is_err());
            storage(address, U256::from(3));
        });
        assert_eq!(
            outer.merge(AccessList::default(), |_| false).0[0].storage_keys,
            vec![B256::from(U256::from(1)), B256::from(U256::from(3))]
        );
        assert!(capture(|| ()).1.merge(AccessList::default(), |_| false).0.is_empty());
    }
}
