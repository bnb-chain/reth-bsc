//! ABI decoding as Solidity's external decoder does it, and the encoders the
//! events and return values use. Ported from go-bsc's core/vm/cas20_abi.go.

use super::errors::{revert, R};
use alloy_primitives::{Address, B256, U256};

pub(crate) fn read_word(args: &[u8], i: usize) -> R<B256> {
    let off = i * 32;
    if args.len() < off + 32 {
        return Err(revert());
    }
    Ok(B256::from_slice(&args[off..off + 32]))
}

pub(crate) fn read_address(args: &[u8], i: usize) -> R<Address> {
    address_from_word(read_word(args, i)?).ok_or_else(revert)
}

/// The check Solidity's external decoder makes for every type narrower than a
/// word: dirty high bytes are a malformed encoding, not a value.
pub(crate) fn word_fits_in(w: B256, n: usize) -> bool {
    w.0[..32 - n].iter().all(|&b| b == 0)
}

pub(crate) fn u64_from_word(w: B256) -> Option<u64> {
    if !word_fits_in(w, 8) {
        return None;
    }
    Some(u64::from_be_bytes(w.0[24..].try_into().unwrap()))
}

pub(crate) fn address_from_word(w: B256) -> Option<Address> {
    if !word_fits_in(w, 20) {
        return None;
    }
    Some(Address::from_slice(&w.0[12..]))
}

pub(crate) fn read_u64(args: &[u8], i: usize) -> R<u64> {
    u64_from_word(read_word(args, i)?).ok_or_else(revert)
}

pub(crate) fn read_u256(args: &[u8], i: usize) -> R<U256> {
    Ok(U256::from_be_bytes(read_word(args, i)?.0))
}

pub(crate) fn read_strict_uint8(args: &[u8], i: usize) -> R<u8> {
    let w = read_word(args, i)?;
    if !is_enum_word(w, 0xff) {
        return Err(revert());
    }
    Ok(w.0[31])
}

pub(crate) fn is_enum_word(w: B256, max: u8) -> bool {
    word_fits_in(w, 1) && w.0[31] <= max
}

pub(crate) fn enc_u256(v: U256) -> Vec<u8> {
    v.to_be_bytes::<32>().to_vec()
}

pub(crate) fn enc_word(w: B256) -> Vec<u8> {
    w.to_vec()
}

pub(crate) fn enc_bool(b: bool) -> Vec<u8> {
    let mut out = vec![0u8; 32];
    if b {
        out[31] = 1;
    }
    out
}

pub(crate) fn enc_string(s: &[u8]) -> Vec<u8> {
    encode_tuple(&[abi_string(s)])
}

// --- ABI encoding primitives ------------------------------------------------

pub(crate) enum AbiPart {
    Word(B256),
    Dynamic(Vec<u8>),
}

pub(crate) fn abi_word(w: B256) -> AbiPart {
    AbiPart::Word(w)
}

pub(crate) fn abi_bytes(b: &[u8]) -> AbiPart {
    let padded = b.len().div_ceil(32) * 32;
    let mut tail = vec![0u8; 32 + padded];
    tail[..32].copy_from_slice(&U256::from(b.len()).to_be_bytes::<32>());
    tail[32..32 + b.len()].copy_from_slice(b);
    AbiPart::Dynamic(tail)
}

pub(crate) fn abi_string(s: &[u8]) -> AbiPart {
    abi_bytes(s)
}

pub(crate) fn abi_word_array(words: &[B256]) -> AbiPart {
    let mut tail = Vec::with_capacity(32 * (words.len() + 1));
    tail.extend_from_slice(&U256::from(words.len()).to_be_bytes::<32>());
    for w in words {
        tail.extend_from_slice(w.as_slice());
    }
    AbiPart::Dynamic(tail)
}

pub(crate) fn encode_tuple(parts: &[AbiPart]) -> Vec<u8> {
    let mut head = Vec::with_capacity(32 * parts.len());
    let mut tail = Vec::new();
    let tail_start = 32 * parts.len();
    for p in parts {
        match p {
            AbiPart::Word(w) => head.extend_from_slice(w.as_slice()),
            AbiPart::Dynamic(t) => {
                head.extend_from_slice(&U256::from(tail_start + tail.len()).to_be_bytes::<32>());
                tail.extend_from_slice(t);
            }
        }
    }
    head.extend(tail);
    head
}

/// abi.encode of one dynamic struct wraps it in a one-element tuple, so the result
/// opens with an offset word (0x20) before the struct's own head/tail.
pub(crate) fn abi_encode_struct(members: &[AbiPart]) -> Vec<u8> {
    encode_tuple(&[AbiPart::Dynamic(encode_tuple(members))])
}

pub(crate) fn read_bytes_arg<'a>(args: &'a [u8], arg_index: usize) -> R<&'a [u8]> {
    read_string_bytes(args, arg_index)
}

/// A string argument is bytes on the wire; the reference client never validates
/// UTF-8, so strings stay byte slices throughout.
pub(crate) fn read_string_arg<'a>(args: &'a [u8], arg_index: usize) -> R<&'a [u8]> {
    read_string_bytes(args, arg_index)
}

fn read_string_bytes<'a>(args: &'a [u8], arg_index: usize) -> R<&'a [u8]> {
    let len = args.len() as u64;
    let off = word_u64(args, arg_index as u64 * 32).ok_or_else(revert)?;
    if off > len || len - off < 32 {
        return Err(revert());
    }
    let n = word_u64(args, off).ok_or_else(revert)?;
    let data_pos = off + 32;
    if n > len - data_pos {
        return Err(revert());
    }
    Ok(&args[data_pos as usize..(data_pos + n) as usize])
}

pub(crate) fn read_bytes_array<'a>(args: &'a [u8], arg_index: usize) -> R<Vec<&'a [u8]>> {
    let len = args.len() as u64;
    let base = word_u64(args, arg_index as u64 * 32).ok_or_else(revert)?;
    if base > len || len - base < 32 {
        return Err(revert());
    }
    let n = word_u64(args, base).ok_or_else(revert)?;
    let arr_data = base + 32;
    if n > (len - arr_data) / 32 {
        return Err(revert());
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let elem_off = word_u64(args, arr_data + i * 32).ok_or_else(revert)?;
        let pos = arr_data.saturating_add(elem_off);
        if elem_off > len - arr_data || pos > len || len - pos < 32 {
            return Err(revert());
        }
        let elem_len = word_u64(args, pos).ok_or_else(revert)?;
        let start = pos + 32;
        if elem_len > len - start {
            return Err(revert());
        }
        out.push(&args[start as usize..(start + elem_len) as usize]);
    }
    Ok(out)
}

/// An offset or length with dirty high bits is a malformed encoding, not a large number.
pub(crate) fn word_u64(args: &[u8], pos: u64) -> Option<u64> {
    let len = args.len() as u64;
    if pos > len || len - pos < 32 {
        return None;
    }
    let pos = pos as usize;
    if args[pos..pos + 24].iter().any(|&b| b != 0) {
        return None;
    }
    Some(u64::from_be_bytes(args[pos + 24..pos + 32].try_into().unwrap()))
}

pub(crate) fn read_word_array(args: &[u8], arg_index: usize) -> R<Vec<B256>> {
    let len = args.len() as u64;
    let base = word_u64(args, arg_index as u64 * 32).ok_or_else(revert)?;
    if base > len || len - base < 32 {
        return Err(revert());
    }
    let n = word_u64(args, base).ok_or_else(revert)?;
    let data_pos = base + 32;
    if n > (len - data_pos) / 32 {
        return Err(revert());
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let p = (data_pos + i * 32) as usize;
        out.push(B256::from_slice(&args[p..p + 32]));
    }
    Ok(out)
}

/// A dynamic uint8[] at head position 0.
pub(crate) fn read_uint8_array(args: &[u8]) -> R<Vec<u8>> {
    let len = args.len() as u64;
    let off = word_u64(args, 0).ok_or_else(revert)?;
    if off > len || len - off < 32 {
        return Err(revert());
    }
    let n = word_u64(args, off).ok_or_else(revert)?;
    let data_pos = off + 32;
    if n > (len - data_pos) / 32 {
        return Err(revert());
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        // Byte-addressed: the caller-supplied head offset need not be 32-aligned.
        let v = word_u64(args, data_pos + i * 32).ok_or_else(revert)?;
        if v > 0xff {
            return Err(revert());
        }
        out.push(v as u8);
    }
    Ok(out)
}
