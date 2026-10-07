//! Credits to <https://github.com/bnb-chain/revm/blob/d66170e712460ae766fc26a063f106658ce33e9d/crates/precompile/src/bls.rs>

use alloy_primitives::Bytes;
use bls_on_arkworks as bls;
use revm::precompile::{
    u64_to_address, Precompile, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
};
use std::{borrow::Cow, vec::Vec};

pub(crate) const BLS_SIGNATURE_VALIDATION: Precompile = Precompile::new(
    PrecompileId::Custom(Cow::Borrowed("BLS_SIGNATURE_VERIFY")),
    u64_to_address(102),
    bls_signature_validation_run,
);

const BLS_MSG_HASH_LENGTH: u64 = 32;
const BLS_SIGNATURE_LENGTH: u64 = 96;
const BLS_SINGLE_PUBKEY_LENGTH: u64 = 48;
const BLS_DST: &[u8] = bls::DST_ETHEREUM.as_bytes();

/// Run bls signature validation precompile.
///
/// The input is encoded as follows:
/// | msg_hash |  signature  |  [{bls pubkey}]  |
/// |    32    |      96     |      [{48}]      |
fn bls_signature_validation_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    let cost = calc_gas_cost(input);
    if cost > gas_limit {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }

    let revert = || Ok(PrecompileOutput::revert(cost, Default::default(), reservoir));

    let msg_and_sig_length = BLS_MSG_HASH_LENGTH + BLS_SIGNATURE_LENGTH;
    let input_length = input.len() as u64;
    if (input_length <= msg_and_sig_length) ||
        !((input_length - msg_and_sig_length).is_multiple_of(BLS_SINGLE_PUBKEY_LENGTH))
    {
        return revert()
    }

    let msg_hash = input[..BLS_MSG_HASH_LENGTH as usize].to_vec();
    let signature = input[BLS_MSG_HASH_LENGTH as usize..msg_and_sig_length as usize].to_vec();

    // check signature format
    if bls::signature_to_point(&signature).is_err() {
        return revert()
    }

    // All keys sign the same message, so sum the validated points before pairing.
    // Preserve the existing key validation, including rejection of infinity, and
    // count repeated keys with their full multiplicity, as go-bsc does.
    let mut aggregate_pubkey = bls::types::G1ProjectivePoint::default();
    for pub_key in
        input[msg_and_sig_length as usize..].chunks_exact(BLS_SINGLE_PUBKEY_LENGTH as usize)
    {
        let Ok(point) = bls::pubkey_to_point(&pub_key.to_vec()) else { return revert() };
        if !bls::pubkey_subgroup_check(point) {
            return revert()
        }
        aggregate_pubkey += point;
    }

    // Hash the message and verify once, including the single-key case. A sum at
    // infinity returns false: the legacy verifier also rejects infinity signatures.
    let aggregate_pubkey = bls::point_to_pubkey(aggregate_pubkey.into());
    let verified = bls::verify(&aggregate_pubkey, &msg_hash, &signature, &BLS_DST.to_vec());
    let output = if verified { Bytes::from_static(&[1]) } else { Bytes::new() };

    Ok(PrecompileOutput::new(cost, output, reservoir))
}

fn calc_gas_cost(input: &[u8]) -> u64 {
    const BLS_SIGNATURE_VALIDATION_BASE: u64 = 1_000;
    const BLS_SIGNATURE_VALIDATION_PER_KER: u64 = 3_500;

    let msg_length = BLS_MSG_HASH_LENGTH + BLS_SIGNATURE_LENGTH;
    let single_pubkey_length = BLS_SINGLE_PUBKEY_LENGTH;
    let input_length = input.len() as u64;

    if (input_length <= msg_length) ||
        !((input_length - msg_length).is_multiple_of(single_pubkey_length))
    {
        return BLS_SIGNATURE_VALIDATION_BASE;
    }

    let pub_key_number = (input_length - msg_length) / single_pubkey_length;

    BLS_SIGNATURE_VALIDATION_BASE + BLS_SIGNATURE_VALIDATION_PER_KER * pub_key_number
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use blst::min_pk::{AggregateSignature, SecretKey};

    // Use a second implementation to construct small, ordinary aggregate signatures.
    fn signed_input(signers: &[u8]) -> Vec<u8> {
        let message = [0x42; 32];
        let keys: Vec<_> =
            signers.iter().map(|seed| SecretKey::key_gen(&[*seed; 32], &[]).unwrap()).collect();
        let signatures: Vec<_> = keys.iter().map(|key| key.sign(&message, BLS_DST, &[])).collect();
        let signature_refs: Vec<_> = signatures.iter().collect();
        let signature =
            AggregateSignature::aggregate(&signature_refs, true).unwrap().to_signature();
        let mut input = message.to_vec();
        input.extend_from_slice(&signature.to_bytes());
        for key in keys {
            input.extend_from_slice(&key.sk_to_pk().to_bytes());
        }
        input
    }

    fn legacy_verifies(input: &[u8]) -> bool {
        let message = input[..32].to_vec();
        let signature = input[32..128].to_vec();
        let keys: Vec<_> = input[128..].chunks_exact(48).map(<[u8]>::to_vec).collect();
        let messages = vec![message; keys.len()];
        bls::aggregate_verify(keys, messages, &signature, &BLS_DST.to_vec())
    }

    #[test]
    fn same_message_aggregates_preserve_results_and_gas() {
        // Repeated keys are valid; each occurrence contributes to the aggregate.
        for signers in
            [&[1][..], &[1, 2], &[1, 2, 3], &[1, 2, 3, 4, 5, 6, 7, 8], &[1, 1], &[1, 2, 1]]
        {
            let mut input = signed_input(signers);
            let cost = 1_000 + 3_500 * signers.len() as u64;
            let reservoir = 73;
            assert!(legacy_verifies(&input));
            assert_eq!(
                bls_signature_validation_run(&input, cost, reservoir).unwrap(),
                PrecompileOutput::new(cost, Bytes::from_static(&[1]), reservoir),
            );
            assert_eq!(
                bls_signature_validation_run(&input, cost - 1, reservoir).unwrap(),
                PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir),
            );

            input[0] ^= 1;
            assert!(!legacy_verifies(&input));
            assert_eq!(
                bls_signature_validation_run(&input, cost, reservoir).unwrap(),
                PrecompileOutput::new(cost, Bytes::new(), reservoir),
            );
        }
    }

    #[test]
    fn invalid_encodings_and_infinity_preserve_revert_behavior() {
        let input = signed_input(&[1, 2]);
        let cost = 8_000;
        let reservoir = 73;
        for (start, len) in [(32, 96), (128, 48), (176, 48)] {
            let mut invalid = input.clone();
            invalid[start..start + len].fill(0);
            assert_eq!(
                bls_signature_validation_run(&invalid, cost, reservoir).unwrap(),
                PrecompileOutput::revert(cost, Bytes::new(), reservoir),
            );

            invalid[start] = 0xc0; // Canonical compressed infinity.
            let expected = if start == 32 {
                // A decodable infinity signature is false, whereas an infinity key reverts.
                PrecompileOutput::new(cost, Bytes::new(), reservoir)
            } else {
                PrecompileOutput::revert(cost, Bytes::new(), reservoir)
            };
            assert_eq!(bls_signature_validation_run(&invalid, cost, reservoir).unwrap(), expected);

            invalid[start + len - 1] = 1; // Non-canonical infinity encoding.
            assert_eq!(
                bls_signature_validation_run(&invalid, cost, reservoir).unwrap(),
                PrecompileOutput::revert(cost, Bytes::new(), reservoir),
            );
        }
    }

    #[test]
    fn cancelling_public_keys_preserve_false_result() {
        let mut input = signed_input(&[1]);
        let point = bls::pubkey_to_point(&input[128..].to_vec()).unwrap();
        input.extend_from_slice(&bls::point_to_pubkey(-point));
        assert!(!legacy_verifies(&input));
        assert_eq!(
            bls_signature_validation_run(&input, 8_000, 73).unwrap(),
            PrecompileOutput::new(8_000, Bytes::new(), 73),
        );

        input[32..128].fill(0);
        input[32] = 0xc0;
        assert!(!legacy_verifies(&input));
        assert_eq!(
            bls_signature_validation_run(&input, 8_000, 73).unwrap(),
            PrecompileOutput::new(8_000, Bytes::new(), 73),
        );
    }

    #[test]
    fn malformed_lengths_preserve_gas_precedence() {
        for len in [0, 32, 128, 175, 177] {
            let input = vec![0; len];
            assert_eq!(
                bls_signature_validation_run(&input, 1_000, 73).unwrap(),
                PrecompileOutput::revert(1_000, Bytes::new(), 73),
            );
            assert_eq!(
                bls_signature_validation_run(&input, 999, 73).unwrap(),
                PrecompileOutput::halt(PrecompileHalt::OutOfGas, 73),
            );
        }
    }

    #[test]
    fn test_bls_signature_validation_with_single_key() {
        let msg_hash = hex!("6377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("8325fccd4ff01e6e0e73de4955d3cb2c6678c6a6abfc465c2991e375c5cf68841ac7847ac51c32a26bd99828bc99f2f6082c41986097e0f6e6711e57c5bd5b18fa6f8f44bf416617cf192a2ff6d4edf0890315d87e3c04f04f0d1611b64bbe0a");
        let pub_key = hex!("a842801f14464ce36470737dc159cb13191e3ad8a49f4f3a38e6a94ea5594ff65753f74661fb7ec944b98fc673bb8230");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key);

        let excepted_output = Bytes::from(vec![1]);
        let result = match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0)
        {
            Ok(o) => o.bytes,
            Err(e) => panic!("BLS signature validation failed, {e:?}"),
        };
        assert_eq!(result, excepted_output);

        // wrong msg hash
        let msg_hash = hex!("1377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("8325fccd4ff01e6e0e73de4955d3cb2c6678c6a6abfc465c2991e375c5cf68841ac7847ac51c32a26bd99828bc99f2f6082c41986097e0f6e6711e57c5bd5b18fa6f8f44bf416617cf192a2ff6d4edf0890315d87e3c04f04f0d1611b64bbe0a");
        let pub_key = hex!("a842801f14464ce36470737dc159cb13191e3ad8a49f4f3a38e6a94ea5594ff65753f74661fb7ec944b98fc673bb8230");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key);

        let excepted_output = Bytes::from(vec![]);
        let result = match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0)
        {
            Ok(o) => o.bytes,
            Err(e) => panic!("BLS signature validation failed, {e:?}"),
        };
        assert_eq!(result, excepted_output);

        // wrong signature
        let msg_hash = hex!("6377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("1325fccd4ff01e6e0e73de4955d3cb2c6678c6a6abfc465c2991e375c5cf68841ac7847ac51c32a26bd99828bc99f2f6082c41986097e0f6e6711e57c5bd5b18fa6f8f44bf416617cf192a2ff6d4edf0890315d87e3c04f04f0d1611b64bbe0a");
        let pub_key = hex!("a842801f14464ce36470737dc159cb13191e3ad8a49f4f3a38e6a94ea5594ff65753f74661fb7ec944b98fc673bb8230");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key);

        match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0) {
            Ok(res) => assert_eq!(res, PrecompileOutput::revert(4500, Default::default(), 0)),
            Err(e) => panic!("BLS signature validation failed, expect error"),
        }

        // wrong pubkey
        let msg_hash = hex!("6377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("1325fccd4ff01e6e0e73de4955d3cb2c6678c6a6abfc465c2991e375c5cf68841ac7847ac51c32a26bd99828bc99f2f6082c41986097e0f6e6711e57c5bd5b18fa6f8f44bf416617cf192a2ff6d4edf0890315d87e3c04f04f0d1611b64bbe0a");
        let pub_key = hex!("1842801f14464ce36470737dc159cb13191e3ad8a49f4f3a38e6a94ea5594ff65753f74661fb7ec944b98fc673bb8230");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key);

        match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0) {
            Ok(res) => assert_eq!(res, PrecompileOutput::revert(4500, Default::default(), 0)),
            Err(e) => panic!("BLS signature validation failed, expect error"),
        }
    }

    #[test]
    fn test_bls_signature_validation_with_multiple_keys() {
        let msg_hash = hex!("6377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("876ac46847e82a2f2887bbeb855916e05bd259086fda5553e4e5e5ee0dcda6869c10e2aa539e265b492015d1bd1b553815a42ea9daf6713b6a0002c6f1aacfa51e55b931745638c0552d7fab4a499bbd6ba71f9be36d35ffa527f77b2a6cebda");
        let pub_key1 = hex!("80223255a26d81a8e1cd94df746f45e87a91d28f408a037804062910b7db68a724cfd204b7f9337bcecac25de86d5515");
        let pub_key2 = hex!("8bec004e938668c67aa0fab6f555282efdf213817e455d9eaa6a0897211eae5f79db5cdc626d1cd5759a0c1c10cf7aa0");
        let pub_key3 = hex!("af952757f442d7240a4cec62de638973a24fde8eb0ad5217be61eea53211c19859c03a299125ea8520f015f6f8865076");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key1);
        input.extend_from_slice(&pub_key2);
        input.extend_from_slice(&pub_key3);

        let excepted_output = Bytes::from(vec![1]);
        let result = match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0)
        {
            Ok(o) => o.bytes,
            Err(e) => panic!("BLS signature validation failed, {e:?}"),
        };
        assert_eq!(result, excepted_output);

        // wrong msg hash
        let msg_hash = hex!("1377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("876ac46847e82a2f2887bbeb855916e05bd259086fda5553e4e5e5ee0dcda6869c10e2aa539e265b492015d1bd1b553815a42ea9daf6713b6a0002c6f1aacfa51e55b931745638c0552d7fab4a499bbd6ba71f9be36d35ffa527f77b2a6cebda");
        let pub_key1 = hex!("80223255a26d81a8e1cd94df746f45e87a91d28f408a037804062910b7db68a724cfd204b7f9337bcecac25de86d5515");
        let pub_key2 = hex!("8bec004e938668c67aa0fab6f555282efdf213817e455d9eaa6a0897211eae5f79db5cdc626d1cd5759a0c1c10cf7aa0");
        let pub_key3 = hex!("af952757f442d7240a4cec62de638973a24fde8eb0ad5217be61eea53211c19859c03a299125ea8520f015f6f8865076");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key1);
        input.extend_from_slice(&pub_key2);
        input.extend_from_slice(&pub_key3);
        let excepted_output = Bytes::from(vec![]);
        let result = match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0)
        {
            Ok(o) => o.bytes,
            Err(e) => panic!("BLS signature validation failed, {e:?}"),
        };
        assert_eq!(result, excepted_output);

        // wrong signature
        let msg_hash = hex!("1377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("276ac46847e82a2f2887bbeb855916e05bd259086fda5553e4e5e5ee0dcda6869c10e2aa539e265b492015d1bd1b553815a42ea9daf6713b6a0002c6f1aacfa51e55b931745638c0552d7fab4a499bbd6ba71f9be36d35ffa527f77b2a6cebda");
        let pub_key1 = hex!("80223255a26d81a8e1cd94df746f45e87a91d28f408a037804062910b7db68a724cfd204b7f9337bcecac25de86d5515");
        let pub_key2 = hex!("8bec004e938668c67aa0fab6f555282efdf213817e455d9eaa6a0897211eae5f79db5cdc626d1cd5759a0c1c10cf7aa0");
        let pub_key3 = hex!("af952757f442d7240a4cec62de638973a24fde8eb0ad5217be61eea53211c19859c03a299125ea8520f015f6f8865076");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key1);
        input.extend_from_slice(&pub_key2);
        input.extend_from_slice(&pub_key3);

        match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0) {
            Ok(res) => assert_eq!(res, PrecompileOutput::revert(11500, Default::default(), 0)),
            Err(e) => panic!("BLS signature validation failed, expect error"),
        }

        // invalid pubkey
        let msg_hash = hex!("1377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("276ac46847e82a2f2887bbeb855916e05bd259086fda5553e4e5e5ee0dcda6869c10e2aa539e265b492015d1bd1b553815a42ea9daf6713b6a0002c6f1aacfa51e55b931745638c0552d7fab4a499bbd6ba71f9be36d35ffa527f77b2a6cebda");
        let pub_key1 = hex!("10223255a26d81a8e1cd94df746f45e87a91d28f408a037804062910b7db68a724cfd204b7f9337bcecac25de86d5515");
        let pub_key2 = hex!("8bec004e938668c67aa0fab6f555282efdf213817e455d9eaa6a0897211eae5f79db5cdc626d1cd5759a0c1c10cf7aa0");
        let pub_key3 = hex!("af952757f442d7240a4cec62de638973a24fde8eb0ad5217be61eea53211c19859c03a299125ea8520f015f6f8865076");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key1);
        input.extend_from_slice(&pub_key2);
        input.extend_from_slice(&pub_key3);

        match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0) {
            Ok(res) => assert_eq!(res, PrecompileOutput::revert(11500, Default::default(), 0)),
            Err(e) => panic!("BLS signature validation failed, expect error"),
        }

        // duplicate pubkey
        let msg_hash = hex!("6377c7e66081cb65e473c1b95db5195a27d04a7108b468890224bedbe1a8a6eb");
        let signature = hex!("876ac46847e82a2f2887bbeb855916e05bd259086fda5553e4e5e5ee0dcda6869c10e2aa539e265b492015d1bd1b553815a42ea9daf6713b6a0002c6f1aacfa51e55b931745638c0552d7fab4a499bbd6ba71f9be36d35ffa527f77b2a6cebda");
        let pub_key1 = hex!("8bec004e938668c67aa0fab6f555282efdf213817e455d9eaa6a0897211eae5f79db5cdc626d1cd5759a0c1c10cf7aa0");
        let pub_key2 = hex!("8bec004e938668c67aa0fab6f555282efdf213817e455d9eaa6a0897211eae5f79db5cdc626d1cd5759a0c1c10cf7aa0");
        let pub_key3 = hex!("af952757f442d7240a4cec62de638973a24fde8eb0ad5217be61eea53211c19859c03a299125ea8520f015f6f8865076");
        let mut input = Vec::<u8>::new();
        input.extend_from_slice(&msg_hash);
        input.extend_from_slice(&signature);
        input.extend_from_slice(&pub_key1);
        input.extend_from_slice(&pub_key2);
        input.extend_from_slice(&pub_key3);
        let excepted_output = Bytes::from(vec![]);
        let result = match bls_signature_validation_run(&Bytes::from(input.clone()), 100_000_000, 0)
        {
            Ok(o) => o.bytes,
            Err(e) => panic!("BLS signature validation failed, {e:?}"),
        };
        assert_eq!(result, excepted_output);
    }
}
