//! helpers for squashing append vecs into ancient append vecs
//! an ancient append vec is:
//! 1. a slot that is older than an epoch old
//! 2. multiple 'slots' squashed into a single older (ie. ancient) slot for convenience and performance
//! Otherwise, an ancient append vec is the same as any other append vec
#![allow(dead_code)]
use {
    crate::{
        accounts_db::{AccountStorageEntry, FoundStoredAccount, SnapshotStorage},
        append_vec::{AppendVec, StoredAccountMeta},
    },
    solana_sdk::{clock::Slot, hash::Hash, pubkey::Pubkey},
    std::{collections::HashMap, sync::Arc},
};

/// a set of accounts need to be stored.
/// If there are too many to fit in 'Primary', the rest are put in 'Overflow'
#[derive(Copy, Clone, Debug)]
pub enum StorageSelector {
    Primary,
    Overflow,
}

/// reference a set of accounts to store
/// The accounts may have to be split between 2 storages (primary and overflow) if there is not enough room in the primary storage.
/// The 'store' functions need data stored in a slice of specific type.
/// We need 1-2 of these slices constructed based on available bytes and individual account sizes.
/// The slice arithmetic accross both hashes and account data gets messy. So, this struct abstracts that.
pub struct AccountsToStore<'a> {
    hashes: Vec<&'a Hash>,
    accounts: Vec<(&'a Pubkey, &'a StoredAccountMeta<'a>, Slot)>,
    /// if 'accounts' contains more items than can be contained in the primary storage, then we have to split these accounts.
    /// 'index_first_item_overflow' specifies the index of the first item in 'accounts' that will go into the overflow storage
    index_first_item_overflow: usize,
}

impl<'a> AccountsToStore<'a> {
    /// break 'stored_accounts' into primary and overflow
    /// available_bytes: how many bytes remain in the primary storage. Excess accounts will be directed to an overflow storage
    pub fn new(
        mut available_bytes: u64,
        stored_accounts: &'a HashMap<Pubkey, FoundStoredAccount>,
        slot: Slot,
    ) -> Self {
        let num_accounts = stored_accounts.len();
        let mut hashes = Vec::with_capacity(num_accounts);
        let mut accounts = Vec::with_capacity(num_accounts);
        // index of the first account that doesn't fit in the current append vec
        let mut index_first_item_overflow = num_accounts; // assume all fit
        stored_accounts.iter().for_each(|account| {
            let account_size = account.1.account_size as u64;
            if available_bytes >= account_size {
                available_bytes = available_bytes.saturating_sub(account_size);
            } else if index_first_item_overflow == num_accounts {
                available_bytes = 0;
                // the # of accounts we have so far seen is the most that will fit in the current ancient append vec
                index_first_item_overflow = hashes.len();
            }
            hashes.push(account.1.account.hash);
            // we have to specify 'slot' here because we are writing to an ancient append vec and squashing slots,
            // so we need to update the previous accounts index entry for this account from 'slot' to 'ancient_slot'
            accounts.push((&account.1.account.meta.pubkey, &account.1.account, slot));
        });
        Self {
            hashes,
            accounts,
            index_first_item_overflow,
        }
    }

    /// get the accounts and hashes to store in the given 'storage'
    pub fn get(
        &self,
        storage: StorageSelector,
    ) -> (
        &[(&'a Pubkey, &'a StoredAccountMeta<'a>, Slot)],
        &[&'a Hash],
    ) {
        let range = match storage {
            StorageSelector::Primary => 0..self.index_first_item_overflow,
            StorageSelector::Overflow => self.index_first_item_overflow..self.accounts.len(),
        };
        (&self.accounts[range.clone()], &self.hashes[range])
    }
}

/// capacity of an ancient append vec
pub fn get_ancient_append_vec_capacity() -> u64 {
    use crate::append_vec::MAXIMUM_APPEND_VEC_FILE_SIZE;
    // smaller than max by a bit just in case
    // some functions add slop on allocation
    MAXIMUM_APPEND_VEC_FILE_SIZE - 2048
}

/// true iff storage is ancient size and is almost completely full
pub fn is_full_ancient(storage: &AppendVec) -> bool {
    // not sure of slop amount here. Maybe max account size with 10MB data?
    // append vecs can't usually be made entirely full
    let threshold_bytes = 10_000;
    is_ancient(storage) && storage.remaining_bytes() < threshold_bytes
}

/// is this a max-size append vec designed to be used as an ancient append vec?
pub fn is_ancient(storage: &AppendVec) -> bool {
    storage.capacity() >= get_ancient_append_vec_capacity()
}

/// return true if the accounts in this slot should be moved to an ancient append vec
/// otherwise, return false and the caller can skip this slot
/// side effect could be updating 'current_ancient'
pub fn should_move_to_ancient_append_vec(
    all_storages: &SnapshotStorage,
    current_ancient: &mut Option<(Slot, Arc<AccountStorageEntry>)>,
    slot: Slot,
) -> bool {
    if current_ancient.is_none() && all_storages.len() == 1 {
        let first_storage = all_storages.first().unwrap();
        if is_ancient(&first_storage.accounts) {
            if is_full_ancient(&first_storage.accounts) {
                return false; // skip this full ancient append vec completely
            }
            // this slot is ancient and can become the 'current' ancient for other slots to be squashed into
            *current_ancient = Some((slot, Arc::clone(first_storage)));
            return false; // we're done with this slot - this slot IS the ancient append vec
        }
    }
    true
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::{
            accounts_db::{get_temp_accounts_paths, AppendVecId},
            append_vec::{AccountMeta, StoredMeta},
        },
        solana_sdk::account::{AccountSharedData, ReadableAccount},
    };

    #[test]
    fn test_accounts_to_store_simple() {
        let map = vec![].into_iter().collect();
        let slot = 1;
        let accounts_to_store = AccountsToStore::new(0, &map, slot);
        for selector in [StorageSelector::Primary, StorageSelector::Overflow] {
            let (accounts, hash) = accounts_to_store.get(selector);
            assert!(accounts.is_empty());
            assert!(hash.is_empty());
        }
    }

    #[test]
    fn test_accounts_to_store_more() {
        let pubkey = Pubkey::new(&[1; 32]);
        let store_id = AppendVecId::default();
        let account_size = 3;

        let account = AccountSharedData::default();

        let account_meta = AccountMeta {
            lamports: 1,
            owner: Pubkey::new(&[2; 32]),
            executable: false,
            rent_epoch: 0,
        };
        let offset = 3;
        let stored_size = 4;
        let hash = Hash::new(&[2; 32]);
        let stored_meta = StoredMeta {
            /// global write version
            write_version: 0,
            /// key for the account
            pubkey,
            data_len: 43,
        };
        let account = StoredAccountMeta {
            meta: &stored_meta,
            /// account data
            account_meta: &account_meta,
            data: account.data(),
            offset,
            stored_size,
            hash: &hash,
        };
        // let account = StoredAccountMeta::new();
        let found = FoundStoredAccount {
            account,
            store_id,
            account_size,
        };
        let map = vec![(pubkey, found)].into_iter().collect();
        for (selector, available_bytes) in [
            (StorageSelector::Primary, account_size),
            (StorageSelector::Overflow, account_size - 1),
        ] {
            let slot = 1;
            let accounts_to_store = AccountsToStore::new(available_bytes as u64, &map, slot);
            let (accounts, hashes) = accounts_to_store.get(selector);
            assert_eq!(
                accounts,
                map.iter()
                    .map(|(a, b)| (a, &b.account, slot))
                    .collect::<Vec<_>>(),
                "mismatch"
            );
            assert_eq!(hashes, vec![&hash]);
            let (accounts, hash) = accounts_to_store.get(get_opposite(&selector));
            assert!(accounts.is_empty());
            assert!(hash.is_empty());
        }
    }
    fn get_opposite(selector: &StorageSelector) -> StorageSelector {
        match selector {
            StorageSelector::Overflow => StorageSelector::Primary,
            StorageSelector::Primary => StorageSelector::Overflow,
        }
    }

    #[test]
    fn test_get_ancient_append_vec_capacity() {
        assert_eq!(
            get_ancient_append_vec_capacity(),
            crate::append_vec::MAXIMUM_APPEND_VEC_FILE_SIZE - 2048
        );
    }

    #[test]
    fn test_is_ancient() {
        for (size, expected_ancient) in [
            (get_ancient_append_vec_capacity() + 1, true),
            (get_ancient_append_vec_capacity(), true),
            (get_ancient_append_vec_capacity() - 1, false),
        ] {
            let tf = crate::append_vec::test_utils::get_append_vec_path("test_is_ancient");
            let (_temp_dirs, _paths) = get_temp_accounts_paths(1).unwrap();
            let av = AppendVec::new(&tf.path, true, size as usize);

            assert_eq!(expected_ancient, is_ancient(&av));
            assert!(!is_full_ancient(&av));
        }
    }

    #[test]
    fn test_is_full_ancient() {
        let size = get_ancient_append_vec_capacity();
        let tf = crate::append_vec::test_utils::get_append_vec_path("test_is_ancient");
        let (_temp_dirs, _paths) = get_temp_accounts_paths(1).unwrap();
        let av = AppendVec::new(&tf.path, true, size as usize);
        assert!(is_ancient(&av));
        assert!(!is_full_ancient(&av));
        let overhead = 400;
        let data_len = size - overhead;
        let mut account = AccountSharedData::default();
        account.set_data(vec![0; data_len as usize]);

        let sm = StoredMeta {
            write_version: 0,
            pubkey: Pubkey::new(&[0; 32]),
            data_len: data_len as u64,
        };
        av.append_accounts(&[(sm, Some(&account))], &[Hash::default()]);
        assert!(is_ancient(&av));
        assert!(is_full_ancient(&av), "Remaining: {}", av.remaining_bytes());
    }
}
