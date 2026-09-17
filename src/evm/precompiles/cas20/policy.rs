//! PolicyRegistry: a chain-shared allow/block-list registry. A policy is a set of
//! addresses plus a type, referenced by tokens via a self-describing uint64 id
//! (high byte = type, low 56 bits = global counter). Reads never revert (they sit
//! on every transfer's hot path); writes are admin-gated. Ported from
//! core/vm/cas20_policy.go.

use super::{
    abi::*,
    activation::ensure_feature_activated,
    ctx::Ctx,
    errors::*,
    sigs::*,
    storage::{addr_key, offset_slot, Store},
    POLICY_REGISTRY_ADDRESS,
};
use alloy_primitives::{Address, B256, U256};

pub(crate) const NAMESPACE: &str = "bsc.policy_registry";

pub(crate) const TYPE_BLOCKLIST: u8 = 0;
pub(crate) const TYPE_ALLOWLIST: u8 = 1;
/// authorized by ANY child
pub(crate) const TYPE_UNION: u8 = 2;
/// authorized by EVERY child
pub(crate) const TYPE_INTERSECT: u8 = 3;
pub(crate) const BATCH_MAX: usize = 64;

pub(crate) const COMPOSITE_MIN_CHILDREN: usize = 2;
pub(crate) const COMPOSITE_MAX_CHILDREN: usize = 4;
/// counters 0 and 1 belong to the two sentinels
pub(crate) const FIRST_ID: u64 = 2;

/// blocklist type, empty -> allow all
pub(crate) const ALWAYS_ALLOW: u64 = 0;
/// allowlist type, empty -> block all
pub(crate) const ALWAYS_BLOCK: u64 = 1 << 56 | 1;

pub(crate) const COUNTER_MAX: u64 = (1 << 56) - 1;

// Storage layout. Slots are append-only across forks.
/// mapping(uint64 => packed word)
pub(crate) const SLOT_POLICIES: u64 = 0;
/// mapping(uint64 => mapping(address => bool))
pub(crate) const SLOT_MEMBERS: u64 = 1;
/// mapping(uint64 => address)
pub(crate) const SLOT_PENDING_ADMINS: u64 = 2;
/// uint64
pub(crate) const SLOT_COUNTER: u64 = 3;
/// mapping(uint64 => uint64[])
pub(crate) const SLOT_CHILDREN: u64 = 4;

/// Existence and admin share one word: bit 255 exists, bits 159:0 the admin.
const EXISTS_BIT: U256 = U256::from_limbs([0, 0, 0, 1 << 63]);

fn pack_policy(admin: Address) -> U256 {
    U256::from_be_bytes(addr_key(admin).0) | EXISTS_BIT
}

fn word_exists(w: U256) -> bool {
    w.bit(255)
}

fn word_admin(w: U256) -> Address {
    Address::from_word(B256::from(w & U256::from_be_bytes(addr_key(Address::repeat_byte(0xff)).0)))
}

pub(crate) fn id_type(id: u64) -> u8 {
    (id >> 56) as u8
}

fn id_well_formed(id: u64) -> bool {
    id_type(id) <= TYPE_INTERSECT
}

pub(crate) fn is_sentinel_policy(id: u64) -> bool {
    id == ALWAYS_ALLOW || id == ALWAYS_BLOCK
}

/// Whether an id names a UNION or INTERSECT policy.
fn is_composite(id: u64) -> bool {
    let t = id_type(id);
    t == TYPE_UNION || t == TYPE_INTERSECT
}

pub(crate) fn pol_slot(offset: u64) -> U256 {
    offset_slot(ROOT_POLICY, offset)
}

pub(crate) fn id_key(id: u64) -> B256 {
    w_u64(id)
}

fn bool_word(b: bool) -> B256 {
    w_u8(b as u8)
}

/// A gas-metered view over the registry's storage.
pub(crate) struct PolicyReg<'r, 'f, 'a> {
    pub(crate) s: Store<'r, 'f, 'a>,
}

impl<'r, 'f, 'a> PolicyReg<'r, 'f, 'a> {
    pub(crate) fn new(ctx: &'r mut Ctx<'f, 'a>) -> Self {
        Self { s: Store::new(ctx, POLICY_REGISTRY_ADDRESS) }
    }

    fn counter(&mut self) -> u64 {
        self.s.get_word(pol_slot(SLOT_COUNTER)).wrapping_to::<u64>()
    }

    fn set_counter(&mut self, v: u64) {
        self.s.set_word(pol_slot(SLOT_COUNTER), U256::from(v));
    }

    fn policy_word(&mut self, id: u64) -> U256 {
        let slot = self.s.map_slot(pol_slot(SLOT_POLICIES), id_key(id));
        self.s.get_word(slot)
    }

    fn set_policy_admin(&mut self, id: u64, a: Address) {
        let slot = self.s.map_slot(pol_slot(SLOT_POLICIES), id_key(id));
        self.s.set_word(slot, pack_policy(a));
    }

    fn exists(&mut self, id: u64) -> bool {
        word_exists(self.policy_word(id))
    }

    fn admin(&mut self, id: u64) -> Address {
        word_admin(self.policy_word(id))
    }

    fn pending(&mut self, id: u64) -> Address {
        let slot = self.s.map_slot(pol_slot(SLOT_PENDING_ADMINS), id_key(id));
        Address::from_word(B256::from(self.s.get_word(slot)))
    }

    fn set_pending(&mut self, id: u64, a: Address) {
        let slot = self.s.map_slot(pol_slot(SLOT_PENDING_ADMINS), id_key(id));
        self.s.set_word(slot, U256::from_be_bytes(addr_key(a).0));
    }

    fn member(&mut self, id: u64, account: Address) -> bool {
        let inner = self.s.map_slot(pol_slot(SLOT_MEMBERS), id_key(id));
        let slot = self.s.map_slot(inner, addr_key(account));
        !self.s.get_word(slot).is_zero()
    }

    fn set_member(&mut self, id: u64, account: Address, in_set: bool) {
        let inner = self.s.map_slot(pol_slot(SLOT_MEMBERS), id_key(id));
        let slot = self.s.map_slot(inner, addr_key(account));
        self.s.set_word(slot, U256::from(in_set as u8));
    }

    /// Never reverts: it sits on every transfer's path.
    pub(crate) fn is_authorized(&mut self, id: u64, account: Address) -> bool {
        if !id_well_formed(id) {
            return false;
        }
        match id {
            ALWAYS_ALLOW => return true,
            ALWAYS_BLOCK => return false,
            _ => {}
        }
        match id_type(id) {
            TYPE_UNION => {
                for child in self.children(id) {
                    if self.is_authorized(child, account) {
                        return true;
                    }
                }
                return false;
            }
            TYPE_INTERSECT => {
                for child in self.children(id) {
                    if !self.is_authorized(child, account) {
                        return false;
                    }
                }
                return true;
            }
            _ => {}
        }
        let member = self.member(id, account);
        if id_type(id) == TYPE_ALLOWLIST {
            return member;
        }
        !member
    }

    fn children_slot(&mut self, id: u64) -> U256 {
        self.s.map_slot(pol_slot(SLOT_CHILDREN), id_key(id))
    }

    fn children(&mut self, id: u64) -> Vec<u64> {
        let slot = self.children_slot(id);
        let n = self.s.get_word(slot).wrapping_to::<u64>();
        if n == 0 || n > COMPOSITE_MAX_CHILDREN as u64 {
            return Vec::new();
        }
        let base = self.s.string_data_root(slot);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            if self.s.ctx.out_of_gas() {
                return Vec::new();
            }
            let w = self.s.get_word(base.wrapping_add(U256::from(i / 4)));
            // Four uint64 lanes per word, LSB-first as Solidity packs them.
            let lane = ((i % 4) * 64) as usize;
            out.push((w >> lane).wrapping_to::<u64>());
        }
        out
    }

    fn set_children(&mut self, id: u64, kids: &[u64]) {
        let slot = self.children_slot(id);
        self.s.set_word(slot, U256::from(kids.len()));
        let base = self.s.string_data_root(slot);
        // Words are rebuilt whole and the loop runs to the count the *maximum* set
        // needs, so a shrink's orphaned tail is cleared as Solidity's array assignment
        // clears it; otherwise the state root would diverge though every read agreed.
        let max_words = COMPOSITE_MAX_CHILDREN.div_ceil(4);
        for w in 0..max_words {
            let mut packed = U256::ZERO;
            for lane in 0..4 {
                if w * 4 + lane < kids.len() {
                    packed |= U256::from(kids[w * 4 + lane]) << (lane * 64);
                }
            }
            let slot_w = base.wrapping_add(U256::from(w));
            if packed.is_zero() && w * 4 >= kids.len() {
                // Only clear a word that holds something: a fresh composite pays for no empty slots.
                if self.s.get_word(slot_w).is_zero() {
                    continue;
                }
            }
            self.s.set_word(slot_w, packed);
        }
    }

    pub(crate) fn policy_exists(&mut self, id: u64) -> bool {
        if !id_well_formed(id) {
            return false;
        }
        if is_sentinel_policy(id) {
            return true;
        }
        self.exists(id)
    }

    fn policy_admin_of(&mut self, id: u64) -> Address {
        if !id_well_formed(id) {
            return Address::ZERO;
        }
        let w = self.policy_word(id);
        if !word_exists(w) {
            return Address::ZERO;
        }
        word_admin(w)
    }

    fn pending_policy_admin_of(&mut self, id: u64) -> Address {
        if !id_well_formed(id) || is_sentinel_policy(id) {
            return Address::ZERO;
        }
        self.pending(id)
    }

    /// Gates on the counter, not the sentinel words, so a harness that pre-warms the
    /// account's bytecode cannot make the seeding skip.
    fn ensure_initialized(&mut self) -> u64 {
        let c = self.counter();
        if c >= FIRST_ID {
            return c;
        }
        self.set_policy_admin(ALWAYS_ALLOW, Address::ZERO);
        self.set_policy_admin(ALWAYS_BLOCK, Address::ZERO);
        self.set_counter(FIRST_ID);
        FIRST_ID
    }

    fn caller(&self) -> Address {
        self.s.ctx.caller
    }

    fn add_log(&mut self, topics: Vec<B256>, data: Vec<u8>) -> bool {
        self.s.ctx.add_log(topics, data)
    }
}

pub(crate) fn run_policy(ctx: &mut Ctx<'_, '_>, input: &[u8]) -> R<Vec<u8>> {
    if input.len() < 4 {
        return Err(revert());
    }
    let sel: Selector = input[..4].try_into().unwrap();
    let args = &input[4..];
    let mut reg = PolicyReg::new(ctx);

    match sel {
        // reads (allowed in read-only frames, never revert on lookup)
        SEL_IS_AUTHORIZED => {
            let id = read_u64(args, 0)?;
            let acct = read_address(args, 1)?;
            return Ok(enc_bool(reg.is_authorized(id, acct)));
        }
        SEL_POLICY_EXISTS => {
            let id = read_u64(args, 0)?;
            return Ok(enc_bool(reg.policy_exists(id)));
        }
        SEL_POLICY_ADMIN => {
            let id = read_u64(args, 0)?;
            return Ok(enc_word(addr_key(reg.policy_admin_of(id))));
        }
        SEL_PENDING_POLICY_ADMIN => {
            let id = read_u64(args, 0)?;
            return Ok(enc_word(addr_key(reg.pending_policy_admin_of(id))));
        }
        SEL_MIN_COMPOSITE_CHILDREN => return Ok(enc_u256(U256::from(COMPOSITE_MIN_CHILDREN))),
        SEL_MAX_COMPOSITE_CHILDREN => return Ok(enc_u256(U256::from(COMPOSITE_MAX_CHILDREN))),
        SEL_COMPOSITE_CHILD_IDS => {
            let id = read_u64(args, 0)?;
            let words: Vec<B256> = reg.children(id).into_iter().map(w_u64).collect();
            return Ok(encode_tuple(&[abi_word_array(&words)]));
        }
        _ => {}
    }

    // Writes: unknown selector, then static frame, then inactive feature, before
    // decoding arguments. The order is consensus-visible.
    match sel {
        SEL_CREATE_POLICY
        | SEL_CREATE_POLICY_WITH_ACCOUNTS
        | SEL_UPDATE_ALLOWLIST
        | SEL_UPDATE_BLOCKLIST
        | SEL_STAGE_UPDATE_ADMIN
        | SEL_FINALIZE_UPDATE_ADMIN
        | SEL_RENOUNCE_ADMIN
        | SEL_CREATE_COMPOSITE
        | SEL_UPDATE_COMPOSITE => {
            if reg.s.ctx.read_only {
                return Err(Cas20Err::WriteProtection);
            }
            ensure_feature_activated(reg.s.ctx, FEATURE_POLICY_REGISTRY)?;
        }
        _ => return Err(revert()), // unknown selector
    }

    match sel {
        SEL_CREATE_POLICY => create_policy(&mut reg, args, false),
        SEL_CREATE_POLICY_WITH_ACCOUNTS => create_policy(&mut reg, args, true),
        SEL_CREATE_COMPOSITE => create_composite_policy(&mut reg, args),
        SEL_UPDATE_COMPOSITE => update_composite(&mut reg, args).map(|_| Vec::new()),
        SEL_UPDATE_ALLOWLIST => update_members(&mut reg, args, TYPE_ALLOWLIST).map(|_| Vec::new()),
        SEL_UPDATE_BLOCKLIST => update_members(&mut reg, args, TYPE_BLOCKLIST).map(|_| Vec::new()),
        SEL_STAGE_UPDATE_ADMIN => stage_update_admin(&mut reg, args).map(|_| Vec::new()),
        SEL_FINALIZE_UPDATE_ADMIN => finalize_update_admin(&mut reg, args).map(|_| Vec::new()),
        SEL_RENOUNCE_ADMIN => renounce_admin(&mut reg, args).map(|_| Vec::new()),
        _ => Err(revert()), // unreachable: the gate above is exhaustive
    }
}

/// One event for creation, handover and renunciation: an admin history is one filter.
fn emit_policy_admin_updated(
    reg: &mut PolicyReg<'_, '_, '_>,
    id: u64,
    previous: Address,
    next: Address,
) -> bool {
    reg.add_log(
        vec![TOPIC_POLICY_ADMIN_UPDATED, id_key(id), addr_key(previous), addr_key(next)],
        Vec::new(),
    )
}

fn emit_members_updated(
    reg: &mut PolicyReg<'_, '_, '_>,
    ptype: u8,
    id: u64,
    updater: Address,
    included: bool,
    accounts: &[B256],
) -> bool {
    let topic =
        if ptype == TYPE_ALLOWLIST { TOPIC_ALLOWLIST_UPDATED } else { TOPIC_BLOCKLIST_UPDATED };
    reg.add_log(
        vec![topic, id_key(id), addr_key(updater)],
        encode_tuple(&[abi_word(bool_word(included)), abi_word_array(accounts)]),
    )
}

/// The count, then existence over the whole set, then eligibility.
fn validate_children(reg: &mut PolicyReg<'_, '_, '_>, kids: &[B256]) -> R<Vec<u64>> {
    if kids.len() < COMPOSITE_MIN_CHILDREN || kids.len() > COMPOSITE_MAX_CHILDREN {
        return Err(rev(ERR_CHILD_POLICIES_OUTSIDE_OF_RANGE, &[]));
    }
    let mut out = Vec::with_capacity(kids.len());
    for w in kids {
        out.push(u64_from_word(*w).ok_or_else(revert)?);
    }
    // Existence for every child before eligibility for any: the order is consensus.
    for &id in &out {
        if !reg.policy_exists(id) {
            return Err(rev(ERR_POLICY_NOT_FOUND, &[]));
        }
    }
    for &id in &out {
        if is_sentinel_policy(id) || is_composite(id) {
            return Err(rev(ERR_INVALID_CHILD_POLICY, &[w_u64(id)]));
        }
    }
    Ok(out)
}

fn emit_composite_updated(
    reg: &mut PolicyReg<'_, '_, '_>,
    id: u64,
    admin: Address,
    kids: &[u64],
) -> bool {
    let words: Vec<B256> = kids.iter().map(|&k| w_u64(k)).collect();
    reg.add_log(
        vec![TOPIC_COMPOSITE_POLICY_UPDATED, id_key(id), addr_key(admin)],
        encode_tuple(&[abi_word_array(&words)]),
    )
}

fn create_composite_policy(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8]) -> R<Vec<u8>> {
    let admin = read_address(args, 0)?;
    let ptype_word = read_word(args, 1)?;
    if !is_enum_word(ptype_word, TYPE_INTERSECT) {
        return Err(revert());
    }
    let ptype = ptype_word.0[31];
    if admin.is_zero() {
        return Err(rev(ERR_ZERO_ADDRESS, &[]));
    }
    if !is_composite((ptype as u64) << 56) {
        return Err(rev(ERR_INCOMPATIBLE_POLICY_TYPE, &[]));
    }
    let raw_kids = read_word_array(args, 2)?;
    let kids = validate_children(reg, &raw_kids)?;

    let c = reg.ensure_initialized();
    if c >= COUNTER_MAX {
        return Err(rev_panic(0x11));
    }
    let id = (ptype as u64) << 56 | c;
    reg.set_counter(c + 1);
    reg.set_policy_admin(id, admin);
    reg.set_children(id, &kids);
    let caller = reg.caller();
    if !reg.add_log(vec![TOPIC_POLICY_CREATED, id_key(id), addr_key(caller)], w_u8(ptype).to_vec())
    {
        return Err(Cas20Err::OutOfGas);
    }
    if !emit_policy_admin_updated(reg, id, Address::ZERO, admin) {
        return Err(Cas20Err::OutOfGas);
    }
    if !emit_composite_updated(reg, id, admin, &kids) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(w_u64(id).to_vec())
}

fn update_composite(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8]) -> R<()> {
    let id = read_u64(args, 0)?;
    if !reg.policy_exists(id) {
        return Err(rev(ERR_POLICY_NOT_FOUND, &[]));
    }
    if !is_composite(id) {
        return Err(rev(ERR_INCOMPATIBLE_POLICY_TYPE, &[]));
    }
    let admin = reg.admin(id);
    if admin.is_zero() || admin != reg.caller() {
        return Err(rev(ERR_UNAUTHORIZED, &[]));
    }
    let raw_kids = read_word_array(args, 1)?;
    let kids = validate_children(reg, &raw_kids)?;
    reg.set_children(id, &kids);
    let caller = reg.caller();
    if !emit_composite_updated(reg, id, caller, &kids) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn create_policy(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8], with_accounts: bool) -> R<Vec<u8>> {
    let admin = read_address(args, 0)?;
    let ptype_word = read_word(args, 1)?;
    let ptype = ptype_word.0[31];
    // The enum widened, so 2 and 3 decode; refused here after the zero-admin check.
    if !is_enum_word(ptype_word, TYPE_INTERSECT) {
        return Err(revert());
    }
    if admin.is_zero() {
        return Err(rev(ERR_ZERO_ADDRESS, &[]));
    }
    if is_composite((ptype as u64) << 56) {
        return Err(rev(ERR_INCOMPATIBLE_POLICY_TYPE, &[]));
    }

    // Decoded and bounded before any write: a revert would not refund gas metered on premature writes.
    let mut accounts = Vec::new();
    if with_accounts {
        accounts = read_word_array(args, 2)?;
        if accounts.len() > BATCH_MAX {
            return Err(rev(ERR_BATCH_SIZE_TOO_LARGE, &[w_u64(BATCH_MAX as u64)]));
        }
    }

    let c = reg.ensure_initialized();
    if c >= COUNTER_MAX {
        return Err(rev_panic(0x11));
    }
    let id = (ptype as u64) << 56 | c;
    reg.set_counter(c + 1);
    reg.set_policy_admin(id, admin);
    let caller = reg.caller();
    if !reg.add_log(vec![TOPIC_POLICY_CREATED, id_key(id), addr_key(caller)], w_u8(ptype).to_vec())
    {
        return Err(Cas20Err::OutOfGas);
    }
    if !emit_policy_admin_updated(reg, id, Address::ZERO, admin) {
        return Err(Cas20Err::OutOfGas);
    }

    if with_accounts {
        for a in &accounts {
            let addr = address_from_word(*a).ok_or_else(revert)?;
            reg.set_member(id, addr, true);
        }
        if !emit_members_updated(reg, ptype, id, caller, true, &accounts) {
            return Err(Cas20Err::OutOfGas);
        }
    }
    Ok(enc_u256(U256::from(id)))
}

fn update_members(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8], want_type: u8) -> R<()> {
    let pid = read_u64(args, 0)?;
    let in_word = read_word(args, 1)?;
    // Decoded before the policy checks, as Solidity's external decoder would have.
    if !is_enum_word(in_word, 1) {
        return Err(revert());
    }
    let accounts = read_word_array(args, 2)?;
    require_policy_exists(reg, pid)?;
    if id_type(pid) != want_type {
        return Err(rev(ERR_INCOMPATIBLE_POLICY_TYPE, &[]));
    }
    require_policy_admin(reg, pid)?;
    if accounts.len() > BATCH_MAX {
        return Err(rev(ERR_BATCH_SIZE_TOO_LARGE, &[w_u64(BATCH_MAX as u64)]));
    }
    let in_set = in_word.0[31] == 1;
    for a in &accounts {
        let addr = address_from_word(*a).ok_or_else(revert)?;
        reg.set_member(pid, addr, in_set);
    }
    let caller = reg.caller();
    if !emit_members_updated(reg, want_type, pid, caller, in_set, &accounts) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn stage_update_admin(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8]) -> R<()> {
    let id = read_u64(args, 0)?;
    let new_admin = read_address(args, 1)?;
    require_policy_admin(reg, id)?;
    reg.set_pending(id, new_admin);
    let caller = reg.caller();
    if !reg.add_log(
        vec![TOPIC_POLICY_ADMIN_STAGED, id_key(id), addr_key(caller), addr_key(new_admin)],
        Vec::new(),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn finalize_update_admin(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8]) -> R<()> {
    let pid = read_u64(args, 0)?;
    require_policy_exists(reg, pid)?;
    let pending = reg.pending(pid);
    if pending.is_zero() {
        return Err(rev(ERR_NO_PENDING_ADMIN, &[]));
    }
    let caller = reg.caller();
    if pending != caller {
        return Err(rev(ERR_UNAUTHORIZED, &[]));
    }
    let previous = reg.admin(pid);
    reg.set_policy_admin(pid, caller);
    reg.set_pending(pid, Address::ZERO);
    if !emit_policy_admin_updated(reg, pid, previous, caller) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn renounce_admin(reg: &mut PolicyReg<'_, '_, '_>, args: &[u8]) -> R<()> {
    let pid = read_u64(args, 0)?;
    require_policy_admin(reg, pid)?;
    // Frozen, not deleted: the exists bit stays, so a renounced policy is not one never created.
    reg.set_policy_admin(pid, Address::ZERO);
    reg.set_pending(pid, Address::ZERO);
    let caller = reg.caller();
    if !emit_policy_admin_updated(reg, pid, caller, Address::ZERO) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn require_policy_exists(reg: &mut PolicyReg<'_, '_, '_>, id: u64) -> R<()> {
    if !reg.policy_exists(id) {
        return Err(rev(ERR_POLICY_NOT_FOUND, &[]));
    }
    Ok(())
}

fn require_policy_admin(reg: &mut PolicyReg<'_, '_, '_>, id: u64) -> R<()> {
    require_policy_exists(reg, id)?;
    let admin = reg.admin(id);
    if admin.is_zero() || admin != reg.caller() {
        return Err(rev(ERR_UNAUTHORIZED, &[]));
    }
    Ok(())
}
