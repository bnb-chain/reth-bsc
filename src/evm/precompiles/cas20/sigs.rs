//! Every selector, topic and derived word the reference client (go-bsc) uses, as
//! literals: the test at the bottom recomputes each one from its preimage, so the
//! literal, the preimage and the Go value cannot drift apart.

use alloy_primitives::{b256, hex, B256};

pub(crate) type Selector = [u8; 4];

// --- function selectors ---
pub(crate) const SEL_BURN_ROLE: Selector = hex!("b930908f");
pub(crate) const SEL_DEFAULT_ADMIN_ROLE: Selector = hex!("a217fddf");
pub(crate) const SEL_DOMAIN_SEPARATOR: Selector = hex!("3644e515");
pub(crate) const SEL_MAX_COMPOSITE_CHILDREN: Selector = hex!("54309870");
pub(crate) const SEL_MAX_UI_MULTIPLIER: Selector = hex!("785c0cf0");
pub(crate) const SEL_METADATA_ROLE: Selector = hex!("38841782");
pub(crate) const SEL_MINT_RECEIVER_SCOPE: Selector = hex!("6e5b013d");
pub(crate) const SEL_MINT_ROLE: Selector = hex!("e9a9c850");
pub(crate) const SEL_MIN_COMPOSITE_CHILDREN: Selector = hex!("b3ae29f7");
pub(crate) const SEL_OPERATOR_ROLE: Selector = hex!("f5b541a6");
pub(crate) const SEL_PAUSE_ROLE: Selector = hex!("389ed267");
pub(crate) const SEL_SEIZE_EXEMPT_SCOPE: Selector = hex!("feb346ec");
pub(crate) const SEL_SEIZE_RECEIVER_SCOPE: Selector = hex!("b31da27f");
pub(crate) const SEL_SEIZE_ROLE: Selector = hex!("3c7e9ba5");
pub(crate) const SEL_TRANSFER_EXECUTOR_SCOPE: Selector = hex!("724e9c53");
pub(crate) const SEL_TRANSFER_RECEIVER_SCOPE: Selector = hex!("210f521b");
pub(crate) const SEL_TRANSFER_SENDER_SCOPE: Selector = hex!("d116fc21");
pub(crate) const SEL_UNPAUSE_ROLE: Selector = hex!("309756fb");
pub(crate) const SEL_WAD_PRECISION: Selector = hex!("664808a8");
pub(crate) const SEL_ACTIVATE: Selector = hex!("59db6e85");
pub(crate) const SEL_ACTIVATION_ADMIN: Selector = hex!("f851a440");
pub(crate) const SEL_ALLOWANCE: Selector = hex!("dd62ed3e");
pub(crate) const SEL_ANNOUNCE: Selector = hex!("595135dd");
pub(crate) const SEL_APPROVE: Selector = hex!("095ea7b3");
pub(crate) const SEL_BALANCE_OF: Selector = hex!("70a08231");
pub(crate) const SEL_BALANCE_OF_UI: Selector = hex!("437a9958");
pub(crate) const SEL_BATCH_MINT: Selector = hex!("68573107");
pub(crate) const SEL_BURN: Selector = hex!("42966c68");
pub(crate) const SEL_BURN_WITH_MEMO: Selector = hex!("38f23b0b");
pub(crate) const SEL_CANCEL_UI_MULTIPLIER: Selector = hex!("2c97a0f0");
pub(crate) const SEL_CHECK_ACTIVATED: Selector = hex!("de5bfd9b");
pub(crate) const SEL_COMPOSITE_CHILD_IDS: Selector = hex!("7c40df74");
pub(crate) const SEL_CONTRACT_URI: Selector = hex!("e8a3d485");
pub(crate) const SEL_CREATE_CAS20: Selector = hex!("a6e2e6f3");
pub(crate) const SEL_CREATE_COMPOSITE: Selector = hex!("6fdd1491");
pub(crate) const SEL_CREATE_POLICY: Selector = hex!("ca5d55f6");
pub(crate) const SEL_CREATE_POLICY_WITH_ACCOUNTS: Selector = hex!("a2d3044f");
pub(crate) const SEL_CURRENCY: Selector = hex!("e5a6b10f");
pub(crate) const SEL_DEACTIVATE: Selector = hex!("22eee84c");
pub(crate) const SEL_DECIMALS: Selector = hex!("313ce567");
pub(crate) const SEL_EFFECTIVE_AT: Selector = hex!("97a4064f");
pub(crate) const SEL_EIP712_DOMAIN: Selector = hex!("84b0196e");
pub(crate) const SEL_EXTRA_METADATA: Selector = hex!("4ddf9da0");
pub(crate) const SEL_FINALIZE_UPDATE_ADMIN: Selector = hex!("33031a9c");
pub(crate) const SEL_FROM_UI_AMOUNT: Selector = hex!("65cd9b3c");
pub(crate) const SEL_GET_CAS20_ADDRESS: Selector = hex!("8d34473b");
pub(crate) const SEL_GET_ROLE_ADMIN: Selector = hex!("248a9ca3");
pub(crate) const SEL_GRANT_ROLE: Selector = hex!("2f2ff15d");
pub(crate) const SEL_HAS_ROLE: Selector = hex!("91d14854");
pub(crate) const SEL_IS_ACTIVATED: Selector = hex!("ba87af80");
pub(crate) const SEL_IS_ANNOUNCEMENT_ID_USED: Selector = hex!("c0da474e");
pub(crate) const SEL_IS_AUTHORIZED: Selector = hex!("55a1179e");
pub(crate) const SEL_IS_CAS20: Selector = hex!("95bc0b5a");
pub(crate) const SEL_IS_CAS20_INITIALIZED: Selector = hex!("dd90c132");
pub(crate) const SEL_IS_PAUSED: Selector = hex!("bc61e733");
pub(crate) const SEL_MINT: Selector = hex!("40c10f19");
pub(crate) const SEL_MINT_WITH_MEMO: Selector = hex!("e44f0b12");
pub(crate) const SEL_MULTIPLIER: Selector = hex!("1b3ed722");
pub(crate) const SEL_NAME: Selector = hex!("06fdde03");
pub(crate) const SEL_NEW_UI_MULTIPLIER: Selector = hex!("dc767007");
pub(crate) const SEL_NONCES: Selector = hex!("7ecebe00");
pub(crate) const SEL_PAUSE: Selector = hex!("a290249c");
pub(crate) const SEL_PAUSED_FEATURES: Selector = hex!("de9997e3");
pub(crate) const SEL_PENDING_POLICY_ADMIN: Selector = hex!("017548b7");
pub(crate) const SEL_PERMIT: Selector = hex!("d505accf");
pub(crate) const SEL_POLICY_ADMIN: Selector = hex!("09dd0a47");
pub(crate) const SEL_POLICY_EXISTS: Selector = hex!("330f5637");
pub(crate) const SEL_POLICY_ID: Selector = hex!("db3de624");
pub(crate) const SEL_RENOUNCE_ADMIN: Selector = hex!("efdb7fa3");
pub(crate) const SEL_RENOUNCE_LAST_ADMIN: Selector = hex!("6f79e3d7");
pub(crate) const SEL_RENOUNCE_ROLE: Selector = hex!("36568abe");
pub(crate) const SEL_REVOKE_ROLE: Selector = hex!("d547741f");
pub(crate) const SEL_SCALED_BALANCE_OF: Selector = hex!("1da24f3e");
pub(crate) const SEL_SEIZE_WITH_MEMO: Selector = hex!("f916d81b");
pub(crate) const SEL_SET_ROLE_ADMIN: Selector = hex!("1e4e0091");
pub(crate) const SEL_STAGE_UPDATE_ADMIN: Selector = hex!("1d7ae695");
pub(crate) const SEL_SUPPLY_CAP: Selector = hex!("8f770ad0");
pub(crate) const SEL_SUPPORTS_INTERFACE: Selector = hex!("01ffc9a7");
pub(crate) const SEL_SYMBOL: Selector = hex!("95d89b41");
pub(crate) const SEL_TO_RAW_BALANCE: Selector = hex!("0ca06c44");
pub(crate) const SEL_TO_SCALED_BALANCE: Selector = hex!("04f04c99");
pub(crate) const SEL_TO_UI_AMOUNT: Selector = hex!("3248d4ff");
pub(crate) const SEL_TOTAL_SUPPLY: Selector = hex!("18160ddd");
pub(crate) const SEL_TOTAL_SUPPLY_UI: Selector = hex!("9bea6429");
pub(crate) const SEL_TRANSFER: Selector = hex!("a9059cbb");
pub(crate) const SEL_TRANSFER_FROM: Selector = hex!("23b872dd");
pub(crate) const SEL_TRANSFER_FROM_WITH_MEMO: Selector = hex!("929c2539");
pub(crate) const SEL_TRANSFER_FROM_WITH_MEMO_FORMAT: Selector = hex!("a3cbabd7");
pub(crate) const SEL_TRANSFER_WITH_MEMO: Selector = hex!("95777d59");
pub(crate) const SEL_TRANSFER_WITH_MEMO_FORMAT: Selector = hex!("aaa7dcce");
pub(crate) const SEL_UI_MULTIPLIER: Selector = hex!("a60bf13d");
pub(crate) const SEL_UNPAUSE: Selector = hex!("8b93dd63");
pub(crate) const SEL_UPDATE_ALLOWLIST: Selector = hex!("3388fb5b");
pub(crate) const SEL_UPDATE_BLOCKLIST: Selector = hex!("5c4e51b8");
pub(crate) const SEL_UPDATE_COMPOSITE: Selector = hex!("bfe142c0");
pub(crate) const SEL_UPDATE_CONTRACT_URI: Selector = hex!("7e5b1e24");
pub(crate) const SEL_UPDATE_EXTRA_METADATA: Selector = hex!("b2851ef5");
pub(crate) const SEL_UPDATE_MULTIPLIER: Selector = hex!("5ffe6146");
pub(crate) const SEL_UPDATE_NAME: Selector = hex!("84da92a7");
pub(crate) const SEL_UPDATE_PARAM: Selector = hex!("ac431751");
pub(crate) const SEL_UPDATE_POLICY: Selector = hex!("adf9c4ea");
pub(crate) const SEL_UPDATE_SUPPLY_CAP: Selector = hex!("e5a97f07");
pub(crate) const SEL_UPDATE_SYMBOL: Selector = hex!("537f5312");
pub(crate) const SEL_UPDATE_UI_MULTIPLIER: Selector = hex!("628e600f");
pub(crate) const SEL_VARIANT_OF: Selector = hex!("82cfdd52");

// --- custom error selectors ---
pub(crate) const ERR_AC_BAD_CONFIRMATION: Selector = hex!("6697b232");
pub(crate) const ERR_AC_UNAUTHORIZED: Selector = hex!("e2517d3f");
pub(crate) const ERR_ACCOUNT_NOT_SEIZABLE: Selector = hex!("91dbbc8d");
pub(crate) const ERR_ALREADY_ACTIVATED: Selector = hex!("866b0041");
pub(crate) const ERR_ANNOUNCEMENT_ID_ALREADY_USED: Selector = hex!("d10b3c9e");
pub(crate) const ERR_ANNOUNCEMENT_IN_PROGRESS: Selector = hex!("5c5f0829");
pub(crate) const ERR_BATCH_SIZE_TOO_LARGE: Selector = hex!("083e2f67");
pub(crate) const ERR_CHILD_POLICIES_OUTSIDE_OF_RANGE: Selector = hex!("697ec868");
pub(crate) const ERR_CONTRACT_PAUSED: Selector = hex!("fd8c4245");
pub(crate) const ERR_DELEGATE_CALL_NOT_ALLOWED: Selector = hex!("0d89438e");
pub(crate) const ERR_EFFECTIVE_AT_IN_PAST: Selector = hex!("14119cf6");
pub(crate) const ERR_EFFECTIVE_AT_TOO_FAR: Selector = hex!("1ce214fa");
pub(crate) const ERR_EMPTY_BATCH: Selector = hex!("c2e5347d");
pub(crate) const ERR_EMPTY_FEATURE_SET: Selector = hex!("4861ff45");
pub(crate) const ERR_EXPIRED_SIGNATURE: Selector = hex!("bd2a913c");
pub(crate) const ERR_FEATURE_NOT_ACTIVATED: Selector = hex!("b9b2a425");
pub(crate) const ERR_INCOMPATIBLE_POLICY_TYPE: Selector = hex!("f1011ef5");
pub(crate) const ERR_INIT_CALL_FAILED: Selector = hex!("4eae0860");
pub(crate) const ERR_INSUFFICIENT_ALLOWANCE: Selector = hex!("192b9e4e");
pub(crate) const ERR_INSUFFICIENT_BALANCE: Selector = hex!("db42144d");
pub(crate) const ERR_INTERNAL_CALL_FAILED: Selector = hex!("b288a127");
pub(crate) const ERR_INTERNAL_CALL_MALFORMED: Selector = hex!("4e2f143e");
pub(crate) const ERR_INVALID_APPROVER: Selector = hex!("8bc146c4");
pub(crate) const ERR_INVALID_CHILD_POLICY: Selector = hex!("46508ef6");
pub(crate) const ERR_INVALID_CURRENCY: Selector = hex!("997c1de8");
pub(crate) const ERR_INVALID_DECIMALS: Selector = hex!("ca950391");
pub(crate) const ERR_INVALID_FORMAT_ID: Selector = hex!("ab95b961");
pub(crate) const ERR_INVALID_METADATA_KEY: Selector = hex!("86ea3abb");
pub(crate) const ERR_INVALID_MULTIPLIER: Selector = hex!("6f12f3dc");
pub(crate) const ERR_INVALID_RECEIVER: Selector = hex!("9cfea583");
pub(crate) const ERR_INVALID_SENDER: Selector = hex!("4c14f64c");
pub(crate) const ERR_INVALID_SIGNER: Selector = hex!("7ba5ffb5");
pub(crate) const ERR_INVALID_SPENDER: Selector = hex!("4e15efda");
pub(crate) const ERR_INVALID_SUPPLY_CAP: Selector = hex!("0a3780ce");
pub(crate) const ERR_INVALID_VALUE: Selector = hex!("0a5a6041");
pub(crate) const ERR_INVALID_VARIANT: Selector = hex!("f10e8e43");
pub(crate) const ERR_LAST_ADMIN_CANNOT_RENOUNCE: Selector = hex!("361513e7");
pub(crate) const ERR_LENGTH_MISMATCH: Selector = hex!("ab8b67c6");
pub(crate) const ERR_MISSING_REQUIRED_FIELD: Selector = hex!("4a43ae87");
pub(crate) const ERR_NO_PENDING_ADMIN: Selector = hex!("b4539afa");
pub(crate) const ERR_NON_PAYABLE: Selector = hex!("6fb1b0e9");
pub(crate) const ERR_NOT_SOLE_ADMIN: Selector = hex!("2a98e73b");
pub(crate) const ERR_PANIC: Selector = hex!("4e487b71");
pub(crate) const ERR_POLICY_FORBIDS: Selector = hex!("a43fec12");
pub(crate) const ERR_POLICY_NOT_FOUND: Selector = hex!("720caa4f");
pub(crate) const ERR_POLICY_NOT_FOUND_ID: Selector = hex!("cccad523");
pub(crate) const ERR_STATIC_CALL_NOT_ALLOWED: Selector = hex!("beaba5b7");
pub(crate) const ERR_SUPPLY_CAP_EXCEEDED: Selector = hex!("4b344b11");
pub(crate) const ERR_TOKEN_ALREADY_EXISTS: Selector = hex!("15ef3a57");
pub(crate) const ERR_UI_MUL_MISSING: Selector = hex!("a7d6a5ca");
pub(crate) const ERR_UI_MUL_EXISTS: Selector = hex!("4481a68e");
pub(crate) const ERR_UNAUTHORIZED: Selector = hex!("82b42900");
pub(crate) const ERR_UNAUTHORIZED_ADDR: Selector = hex!("8e4a23d6");
pub(crate) const ERR_UNKNOWN_PARAM: Selector = hex!("97b88354");
pub(crate) const ERR_UNSUPPORTED_SCOPE: Selector = hex!("cdd98a4a");
pub(crate) const ERR_UNSUPPORTED_VERSION: Selector = hex!("c0d8b4e0");
pub(crate) const ERR_ZERO_ADDRESS: Selector = hex!("d92e233d");

// --- event topics ---
pub(crate) const TOPIC_TRANSFER: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
pub(crate) const TOPIC_APPROVAL: B256 =
    b256!("8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925");
pub(crate) const TOPIC_ROLE_GRANTED: B256 =
    b256!("2f8788117e7eff1d82e926ec794901d17c78024a50270940304540a733656f0d");
pub(crate) const TOPIC_ROLE_REVOKED: B256 =
    b256!("f6391f5c32d9c69d2a47ea670b442974b53935d1edc7fd64eb21e047a839171b");
pub(crate) const TOPIC_ROLE_ADMIN_CHANGED: B256 =
    b256!("bd79b86ffe0ab8e8776151514217cd7cacd52c909f66475c3af44e129f0b00ff");
pub(crate) const TOPIC_SEIZED: B256 =
    b256!("a9aec5d8b86e2fa2fd6ac3af62f2622e3dfdab1967d4cbbb56a5df7d74cb887c");
pub(crate) const TOPIC_LAST_ADMIN_RENOUNCED: B256 =
    b256!("e8d3a9872e7ca325571ff1e4c51ddd69090a0345240cc605ccde365ec867cc67");
pub(crate) const TOPIC_POLICY_UPDATED: B256 =
    b256!("8b4790f7ff717fc8f60f07ae099e47ef318dc04b37ae98056b50a22b79056626");
pub(crate) const TOPIC_PAUSED: B256 =
    b256!("3e849708b13f20dd0b87a503523c305e7604cd7e8f855e7a5ebe21397dfd7146");
pub(crate) const TOPIC_UNPAUSED: B256 =
    b256!("086a815e5123cf1af5cb7a82f8b052bf96c622204c98b2f4f9782bf77f753c57");
pub(crate) const TOPIC_SUPPLY_CAP_UPDATED: B256 =
    b256!("6d14f44808ce024f263432bc38d019a9951fbe674e9898b54844dbc8dc09c23a");
pub(crate) const TOPIC_NAME_UPDATED: B256 =
    b256!("74321da206c1b9fa34367f7ece59ca49371dcd13820b9a5c3767ae1ecceed51a");
pub(crate) const TOPIC_SYMBOL_UPDATED: B256 =
    b256!("64e8b5c6dcea43dd79766bb3b8af7c45968d12b68c960cf2da23856f34d598d4");
pub(crate) const TOPIC_CONTRACT_URI_UPDATED: B256 =
    b256!("a5d4097edda6d87cb9329af83fb3712ef77eeb13738ffe43cc35a4ce305ad962");
pub(crate) const TOPIC_EIP712_DOMAIN_CHANGED: B256 =
    b256!("0a6387c9ea3628b88a633bb4f3b151770f70085117a15f9bf3787cda53f13d31");
pub(crate) const TOPIC_MEMO: B256 =
    b256!("6989f5818dcfd11f8cd53b27c94cec33dae1589735f03e639cba54553a1825e8");
pub(crate) const TOPIC_MEMO_FORMAT_DECLARED: B256 =
    b256!("18d9dee6045d4b076f44c903257054dec63c9c2ba01b56b909fad8a35e6d17be");
pub(crate) const TOPIC_MULTIPLIER_UPDATED: B256 =
    b256!("4dbe4840d7465bd162f67814cea0b519567a2e0e578bcde61e7f4ced361e5a3d");
pub(crate) const TOPIC_ANNOUNCEMENT: B256 =
    b256!("ccebf8218a62875909564adef86a6f4df81503cb617221e793357d62f8e813f7");
pub(crate) const TOPIC_END_ANNOUNCEMENT: B256 =
    b256!("96d64dafe2c790596430196b982ad1da3221cb3b0f4e6e2df77f2e4f71a90037");
pub(crate) const TOPIC_EXTRA_METADATA_UPDATED: B256 =
    b256!("d7bb345be29e78d635203d40fe0567e7ef19d5cd5cc5fcd25f768b8063e82aa1");
pub(crate) const TOPIC_UI_MULTIPLIER_UPDATED: B256 =
    b256!("2205df4534432b2f60654a3fdb48737ffdaf3e9edb1a498bd985bc026b15b055");
pub(crate) const TOPIC_UI_MULTIPLIER_UPDATE_CANCELLED: B256 =
    b256!("883856335ba5f60c18b9817c4505d3c7d3f6223dcf39516b30c508c46a5e1cad");
pub(crate) const TOPIC_POLICY_CREATED: B256 =
    b256!("718d87917f0c4cfd1263707ef0e77c656ed8d8bfaca06152bdb0b8094142ec27");
pub(crate) const TOPIC_POLICY_ADMIN_STAGED: B256 =
    b256!("dbf3b34a4c956c56ca05cd4b8f9293a4347ad61994445a4b89817d4a19561136");
pub(crate) const TOPIC_POLICY_ADMIN_UPDATED: B256 =
    b256!("98925cfb1bc09c5b43dd0dd56d3d95aa04fb3300927580cc588c3f5dd58c15e1");
pub(crate) const TOPIC_COMPOSITE_POLICY_UPDATED: B256 =
    b256!("4ff6adaab31b0df87aa7b8b7320c52b8b3b5eede3bf28a6baaaa8b8b7e1d6363");
pub(crate) const TOPIC_ALLOWLIST_UPDATED: B256 =
    b256!("18c46532f90187ba11e436e21da087b684801d7f0787f2043f26f079c91e9ef0");
pub(crate) const TOPIC_BLOCKLIST_UPDATED: B256 =
    b256!("2ff63c102b1b9fd7f5d39f83039c5d6aaf50a414a4f2def2704e41be2628f1e3");
pub(crate) const TOPIC_CAS20_CREATED: B256 =
    b256!("c0c726edbec4bf665160de37131bd2309460ebd49941b2e3b6704a74c7900365");
pub(crate) const TOPIC_FEATURE_ACTIVATED: B256 =
    b256!("8c7a0ecdbb8d96e867e43ec1aef80027976ee493c18bef399fa799ed19752451");
pub(crate) const TOPIC_FEATURE_DEACTIVATED: B256 =
    b256!("15bf65a782c3258c63268ba9d7aed710cf9f9315d3687d9a4632ccdad7926c84");
pub(crate) const TOPIC_ADMIN_CHANGED: B256 =
    b256!("4eb572e99196bed0270fbd5b17a948e19c3f50a97838cb0d2a75a823ff8e6c50");
pub(crate) const TOPIC_PARAM_CHANGE: B256 =
    b256!("f1ce9b2cbf50eeb05769a29e2543fd350cab46894a7dd9978a12d534bb20e633");

// --- roles, policy scopes, features, typehashes ---
pub(crate) const ROLE_DEFAULT_ADMIN: B256 = B256::ZERO;
pub(crate) const ROLE_MINT: B256 =
    b256!("154c00819833dac601ee5ddded6fda79d9d8b506b911b3dbd54cdb95fe6c3686");
pub(crate) const ROLE_BURN: B256 =
    b256!("e97b137254058bd94f28d2f3eb79e2d34074ffb488d042e3bc958e0a57d2fa22");
pub(crate) const ROLE_SEIZE: B256 =
    b256!("3469b8b0d89e9604f8510ed143f74a8336d22955d4f83e23bf53d9414e27f432");
pub(crate) const ROLE_PAUSE: B256 =
    b256!("139c2898040ef16910dc9f44dc697df79363da767d8bc92f2e310312b816e46d");
pub(crate) const ROLE_UNPAUSE: B256 =
    b256!("265b220c5a8891efdd9e1b1b7fa72f257bd5169f8d87e319cf3dad6ff52b94ae");
pub(crate) const ROLE_METADATA: B256 =
    b256!("6bd6b5318a46e5fff572d5e4258a20774aab40cc35ac7680654b9081fcc82f80");
pub(crate) const ROLE_OPERATOR: B256 =
    b256!("97667070c54ef182b0f5858b034beac1b6f3089aa2d3188bb1e8929f4fa9b929");
pub(crate) const SCOPE_TRANSFER_SENDER: B256 =
    b256!("b81736c875ab819dd97f59f2a6542cfb731ad52b4ae15a6f24df2fb02b0327f5");
pub(crate) const SCOPE_TRANSFER_RECEIVER: B256 =
    b256!("8a4b3fa2d8b921852bc0089c6ef0958aa6961897be36fd731330fe2cd23f8363");
pub(crate) const SCOPE_TRANSFER_EXECUTOR: B256 =
    b256!("10be5173aff2a44e748bd9acd8b19fe34689581398a9db7ba2fb671e786ff7d8");
pub(crate) const SCOPE_MINT_RECEIVER: B256 =
    b256!("a0d5ae037e66a09119acf080a1d807abb9b6d03b6b9130eb19f7c1e6bdb8ffc8");
pub(crate) const SCOPE_SEIZE_EXEMPT: B256 =
    b256!("edb5da348cfb67af08746d3afd1be81034b50d5c8576f31aff688f39dfd540ed");
pub(crate) const SCOPE_SEIZE_RECEIVER: B256 =
    b256!("bf15b19caf5c77422c038bc25f26b8b815c3a14f6d04c6616076b81bcfe07b3d");
pub(crate) const FEATURE_ASSET: B256 =
    b256!("aad5cc684994ec8b24b61a18b11c81e15b4e634dc316384aecaa401ee0744e83");
pub(crate) const FEATURE_STABLECOIN: B256 =
    b256!("397796e401b7a4d873181dea7977d9e27050faf36d281f9dc858416a405d2b75");
pub(crate) const FEATURE_POLICY_REGISTRY: B256 =
    b256!("cc84b168b7eedbce699f3234b0a635f610f4aa826d72cc6a19f5b9a264557edf");
pub(crate) const DOMAIN_TYPEHASH: B256 =
    b256!("8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f");
pub(crate) const PERMIT_TYPEHASH: B256 =
    b256!("6e71edae12b1b97f4d1f60370fef10105fa2faae0126114a169c64845d6126c9");

// --- ERC-7201 namespace roots ---
pub(crate) const ROOT_CORE: B256 =
    b256!("562aafeb82006b6968827b59606253289ba8bc22c7d434d71765d5b3a068af00");
pub(crate) const ROOT_ASSET: B256 =
    b256!("ff1f981cfcfd1795c7002397f3fa439537651c7544b1d579931b62466060ae00");
pub(crate) const ROOT_STABLECOIN: B256 =
    b256!("d88f59ff5f54ab199d582c5de9fd2a11ddf31b4380c891c97fe80c42868a1600");
pub(crate) const ROOT_POLICY: B256 =
    b256!("2e7731329603a38e578303ba37c039549397ad42853922c474dfc3cb33d7b000");
pub(crate) const ROOT_ACTIVATION: B256 =
    b256!("a8970030726ea4c1e5fe64bf3ba11683da0b59f818265a7369e5570e9fc0bd00");

/// keccak256(0xEF), the code hash every initialized CAS20 account carries.
pub(crate) const MARKER_CODE_HASH: B256 =
    b256!("309b8896ee4c1ff7ec1966155373dee42663b6b40c3fedc70ba501684848d2a3");

/// The function a selector names, for observers and diagnostics; `"unknown"` for
/// one no CAS20 table has.
pub(crate) fn selector_name(sel: Selector) -> &'static str {
    match sel {
        SEL_BURN_ROLE => "BURN_ROLE",
        SEL_DEFAULT_ADMIN_ROLE => "DEFAULT_ADMIN_ROLE",
        SEL_DOMAIN_SEPARATOR => "DOMAIN_SEPARATOR",
        SEL_MAX_COMPOSITE_CHILDREN => "MAX_COMPOSITE_CHILD_POLICIES",
        SEL_MAX_UI_MULTIPLIER => "MAX_UI_MULTIPLIER",
        SEL_METADATA_ROLE => "METADATA_ROLE",
        SEL_MINT_RECEIVER_SCOPE => "MINT_RECEIVER_POLICY",
        SEL_MINT_ROLE => "MINT_ROLE",
        SEL_MIN_COMPOSITE_CHILDREN => "MIN_COMPOSITE_CHILD_POLICIES",
        SEL_OPERATOR_ROLE => "OPERATOR_ROLE",
        SEL_PAUSE_ROLE => "PAUSE_ROLE",
        SEL_SEIZE_EXEMPT_SCOPE => "SEIZE_EXEMPT_POLICY",
        SEL_SEIZE_RECEIVER_SCOPE => "SEIZE_RECEIVER_POLICY",
        SEL_SEIZE_ROLE => "SEIZE_ROLE",
        SEL_TRANSFER_EXECUTOR_SCOPE => "TRANSFER_EXECUTOR_POLICY",
        SEL_TRANSFER_RECEIVER_SCOPE => "TRANSFER_RECEIVER_POLICY",
        SEL_TRANSFER_SENDER_SCOPE => "TRANSFER_SENDER_POLICY",
        SEL_UNPAUSE_ROLE => "UNPAUSE_ROLE",
        SEL_WAD_PRECISION => "WAD_PRECISION",
        SEL_ACTIVATE => "activate",
        SEL_ACTIVATION_ADMIN => "admin",
        SEL_ALLOWANCE => "allowance",
        SEL_ANNOUNCE => "announce",
        SEL_APPROVE => "approve",
        SEL_BALANCE_OF => "balanceOf",
        SEL_BALANCE_OF_UI => "balanceOfUI",
        SEL_BATCH_MINT => "batchMint",
        SEL_BURN => "burn",
        SEL_BURN_WITH_MEMO => "burnWithMemo",
        SEL_CANCEL_UI_MULTIPLIER => "cancelUIMultiplierUpdate",
        SEL_CHECK_ACTIVATED => "checkActivated",
        SEL_COMPOSITE_CHILD_IDS => "compositePolicyChildIds",
        SEL_CONTRACT_URI => "contractURI",
        SEL_CREATE_CAS20 => "createCAS20",
        SEL_CREATE_COMPOSITE => "createCompositePolicy",
        SEL_CREATE_POLICY => "createPolicy",
        SEL_CREATE_POLICY_WITH_ACCOUNTS => "createPolicyWithAccounts",
        SEL_CURRENCY => "currency",
        SEL_DEACTIVATE => "deactivate",
        SEL_DECIMALS => "decimals",
        SEL_EFFECTIVE_AT => "effectiveAt",
        SEL_EIP712_DOMAIN => "eip712Domain",
        SEL_EXTRA_METADATA => "extraMetadata",
        SEL_FINALIZE_UPDATE_ADMIN => "finalizeUpdateAdmin",
        SEL_FROM_UI_AMOUNT => "fromUIAmount",
        SEL_GET_CAS20_ADDRESS => "getCAS20Address",
        SEL_GET_ROLE_ADMIN => "getRoleAdmin",
        SEL_GRANT_ROLE => "grantRole",
        SEL_HAS_ROLE => "hasRole",
        SEL_IS_ACTIVATED => "isActivated",
        SEL_IS_ANNOUNCEMENT_ID_USED => "isAnnouncementIdUsed",
        SEL_IS_AUTHORIZED => "isAuthorized",
        SEL_IS_CAS20 => "isCAS20",
        SEL_IS_CAS20_INITIALIZED => "isCAS20Initialized",
        SEL_IS_PAUSED => "isPaused",
        SEL_MINT => "mint",
        SEL_MINT_WITH_MEMO => "mintWithMemo",
        SEL_MULTIPLIER => "multiplier",
        SEL_NAME => "name",
        SEL_NEW_UI_MULTIPLIER => "newUIMultiplier",
        SEL_NONCES => "nonces",
        SEL_PAUSE => "pause",
        SEL_PAUSED_FEATURES => "pausedFeatures",
        SEL_PENDING_POLICY_ADMIN => "pendingPolicyAdmin",
        SEL_PERMIT => "permit",
        SEL_POLICY_ADMIN => "policyAdmin",
        SEL_POLICY_EXISTS => "policyExists",
        SEL_POLICY_ID => "policyId",
        SEL_RENOUNCE_ADMIN => "renounceAdmin",
        SEL_RENOUNCE_LAST_ADMIN => "renounceLastAdmin",
        SEL_RENOUNCE_ROLE => "renounceRole",
        SEL_REVOKE_ROLE => "revokeRole",
        SEL_SCALED_BALANCE_OF => "scaledBalanceOf",
        SEL_SEIZE_WITH_MEMO => "seizeWithMemo",
        SEL_SET_ROLE_ADMIN => "setRoleAdmin",
        SEL_STAGE_UPDATE_ADMIN => "stageUpdateAdmin",
        SEL_SUPPLY_CAP => "supplyCap",
        SEL_SUPPORTS_INTERFACE => "supportsInterface",
        SEL_SYMBOL => "symbol",
        SEL_TO_RAW_BALANCE => "toRawBalance",
        SEL_TO_SCALED_BALANCE => "toScaledBalance",
        SEL_TO_UI_AMOUNT => "toUIAmount",
        SEL_TOTAL_SUPPLY => "totalSupply",
        SEL_TOTAL_SUPPLY_UI => "totalSupplyUI",
        SEL_TRANSFER => "transfer",
        SEL_TRANSFER_FROM => "transferFrom",
        SEL_TRANSFER_FROM_WITH_MEMO => "transferFromWithMemo",
        SEL_TRANSFER_FROM_WITH_MEMO_FORMAT => "transferFromWithMemoFormat",
        SEL_TRANSFER_WITH_MEMO => "transferWithMemo",
        SEL_TRANSFER_WITH_MEMO_FORMAT => "transferWithMemoFormat",
        SEL_UI_MULTIPLIER => "uiMultiplier",
        SEL_UNPAUSE => "unpause",
        SEL_UPDATE_ALLOWLIST => "updateAllowlist",
        SEL_UPDATE_BLOCKLIST => "updateBlocklist",
        SEL_UPDATE_COMPOSITE => "updateComposite",
        SEL_UPDATE_CONTRACT_URI => "updateContractURI",
        SEL_UPDATE_EXTRA_METADATA => "updateExtraMetadata",
        SEL_UPDATE_MULTIPLIER => "updateMultiplier",
        SEL_UPDATE_NAME => "updateName",
        SEL_UPDATE_PARAM => "updateParam",
        SEL_UPDATE_POLICY => "updatePolicy",
        SEL_UPDATE_SUPPLY_CAP => "updateSupplyCap",
        SEL_UPDATE_SYMBOL => "updateSymbol",
        SEL_UPDATE_UI_MULTIPLIER => "updateUIMultiplier",
        SEL_VARIANT_OF => "variantOf",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::precompiles::cas20::storage::erc7201_root;
    use alloy_primitives::keccak256;

    fn sel(sig: &str) -> Selector {
        keccak256(sig.as_bytes())[..4].try_into().unwrap()
    }

    #[test]
    fn selectors_match_their_signatures() {
        assert_eq!(SEL_BURN_ROLE, sel("BURN_ROLE()"), "BURN_ROLE()");
        assert_eq!(SEL_DEFAULT_ADMIN_ROLE, sel("DEFAULT_ADMIN_ROLE()"), "DEFAULT_ADMIN_ROLE()");
        assert_eq!(SEL_DOMAIN_SEPARATOR, sel("DOMAIN_SEPARATOR()"), "DOMAIN_SEPARATOR()");
        assert_eq!(
            SEL_MAX_COMPOSITE_CHILDREN,
            sel("MAX_COMPOSITE_CHILD_POLICIES()"),
            "MAX_COMPOSITE_CHILD_POLICIES()"
        );
        assert_eq!(SEL_MAX_UI_MULTIPLIER, sel("MAX_UI_MULTIPLIER()"), "MAX_UI_MULTIPLIER()");
        assert_eq!(SEL_METADATA_ROLE, sel("METADATA_ROLE()"), "METADATA_ROLE()");
        assert_eq!(
            SEL_MINT_RECEIVER_SCOPE,
            sel("MINT_RECEIVER_POLICY()"),
            "MINT_RECEIVER_POLICY()"
        );
        assert_eq!(SEL_MINT_ROLE, sel("MINT_ROLE()"), "MINT_ROLE()");
        assert_eq!(
            SEL_MIN_COMPOSITE_CHILDREN,
            sel("MIN_COMPOSITE_CHILD_POLICIES()"),
            "MIN_COMPOSITE_CHILD_POLICIES()"
        );
        assert_eq!(SEL_OPERATOR_ROLE, sel("OPERATOR_ROLE()"), "OPERATOR_ROLE()");
        assert_eq!(SEL_PAUSE_ROLE, sel("PAUSE_ROLE()"), "PAUSE_ROLE()");
        assert_eq!(SEL_SEIZE_EXEMPT_SCOPE, sel("SEIZE_EXEMPT_POLICY()"), "SEIZE_EXEMPT_POLICY()");
        assert_eq!(
            SEL_SEIZE_RECEIVER_SCOPE,
            sel("SEIZE_RECEIVER_POLICY()"),
            "SEIZE_RECEIVER_POLICY()"
        );
        assert_eq!(SEL_SEIZE_ROLE, sel("SEIZE_ROLE()"), "SEIZE_ROLE()");
        assert_eq!(
            SEL_TRANSFER_EXECUTOR_SCOPE,
            sel("TRANSFER_EXECUTOR_POLICY()"),
            "TRANSFER_EXECUTOR_POLICY()"
        );
        assert_eq!(
            SEL_TRANSFER_RECEIVER_SCOPE,
            sel("TRANSFER_RECEIVER_POLICY()"),
            "TRANSFER_RECEIVER_POLICY()"
        );
        assert_eq!(
            SEL_TRANSFER_SENDER_SCOPE,
            sel("TRANSFER_SENDER_POLICY()"),
            "TRANSFER_SENDER_POLICY()"
        );
        assert_eq!(SEL_UNPAUSE_ROLE, sel("UNPAUSE_ROLE()"), "UNPAUSE_ROLE()");
        assert_eq!(SEL_WAD_PRECISION, sel("WAD_PRECISION()"), "WAD_PRECISION()");
        assert_eq!(SEL_ACTIVATE, sel("activate(bytes32)"), "activate(bytes32)");
        assert_eq!(SEL_ACTIVATION_ADMIN, sel("admin()"), "admin()");
        assert_eq!(SEL_ALLOWANCE, sel("allowance(address,address)"), "allowance(address,address)");
        assert_eq!(
            SEL_ANNOUNCE,
            sel("announce(bytes[],string,string,string)"),
            "announce(bytes[],string,string,string)"
        );
        assert_eq!(SEL_APPROVE, sel("approve(address,uint256)"), "approve(address,uint256)");
        assert_eq!(SEL_BALANCE_OF, sel("balanceOf(address)"), "balanceOf(address)");
        assert_eq!(SEL_BALANCE_OF_UI, sel("balanceOfUI(address)"), "balanceOfUI(address)");
        assert_eq!(
            SEL_BATCH_MINT,
            sel("batchMint(address[],uint256[])"),
            "batchMint(address[],uint256[])"
        );
        assert_eq!(SEL_BURN, sel("burn(uint256)"), "burn(uint256)");
        assert_eq!(
            SEL_BURN_WITH_MEMO,
            sel("burnWithMemo(uint256,bytes32)"),
            "burnWithMemo(uint256,bytes32)"
        );
        assert_eq!(
            SEL_CANCEL_UI_MULTIPLIER,
            sel("cancelUIMultiplierUpdate()"),
            "cancelUIMultiplierUpdate()"
        );
        assert_eq!(SEL_CHECK_ACTIVATED, sel("checkActivated(bytes32)"), "checkActivated(bytes32)");
        assert_eq!(
            SEL_COMPOSITE_CHILD_IDS,
            sel("compositePolicyChildIds(uint64)"),
            "compositePolicyChildIds(uint64)"
        );
        assert_eq!(SEL_CONTRACT_URI, sel("contractURI()"), "contractURI()");
        assert_eq!(
            SEL_CREATE_CAS20,
            sel("createCAS20(uint8,bytes32,bytes,bytes[])"),
            "createCAS20(uint8,bytes32,bytes,bytes[])"
        );
        assert_eq!(
            SEL_CREATE_COMPOSITE,
            sel("createCompositePolicy(address,uint8,uint64[])"),
            "createCompositePolicy(address,uint8,uint64[])"
        );
        assert_eq!(
            SEL_CREATE_POLICY,
            sel("createPolicy(address,uint8)"),
            "createPolicy(address,uint8)"
        );
        assert_eq!(
            SEL_CREATE_POLICY_WITH_ACCOUNTS,
            sel("createPolicyWithAccounts(address,uint8,address[])"),
            "createPolicyWithAccounts(address,uint8,address[])"
        );
        assert_eq!(SEL_CURRENCY, sel("currency()"), "currency()");
        assert_eq!(SEL_DEACTIVATE, sel("deactivate(bytes32)"), "deactivate(bytes32)");
        assert_eq!(SEL_DECIMALS, sel("decimals()"), "decimals()");
        assert_eq!(SEL_EFFECTIVE_AT, sel("effectiveAt()"), "effectiveAt()");
        assert_eq!(SEL_EIP712_DOMAIN, sel("eip712Domain()"), "eip712Domain()");
        assert_eq!(SEL_EXTRA_METADATA, sel("extraMetadata(string)"), "extraMetadata(string)");
        assert_eq!(
            SEL_FINALIZE_UPDATE_ADMIN,
            sel("finalizeUpdateAdmin(uint64)"),
            "finalizeUpdateAdmin(uint64)"
        );
        assert_eq!(SEL_FROM_UI_AMOUNT, sel("fromUIAmount(uint256)"), "fromUIAmount(uint256)");
        assert_eq!(
            SEL_GET_CAS20_ADDRESS,
            sel("getCAS20Address(uint8,address,bytes32)"),
            "getCAS20Address(uint8,address,bytes32)"
        );
        assert_eq!(SEL_GET_ROLE_ADMIN, sel("getRoleAdmin(bytes32)"), "getRoleAdmin(bytes32)");
        assert_eq!(SEL_GRANT_ROLE, sel("grantRole(bytes32,address)"), "grantRole(bytes32,address)");
        assert_eq!(SEL_HAS_ROLE, sel("hasRole(bytes32,address)"), "hasRole(bytes32,address)");
        assert_eq!(SEL_IS_ACTIVATED, sel("isActivated(bytes32)"), "isActivated(bytes32)");
        assert_eq!(
            SEL_IS_ANNOUNCEMENT_ID_USED,
            sel("isAnnouncementIdUsed(string)"),
            "isAnnouncementIdUsed(string)"
        );
        assert_eq!(
            SEL_IS_AUTHORIZED,
            sel("isAuthorized(uint64,address)"),
            "isAuthorized(uint64,address)"
        );
        assert_eq!(SEL_IS_CAS20, sel("isCAS20(address)"), "isCAS20(address)");
        assert_eq!(
            SEL_IS_CAS20_INITIALIZED,
            sel("isCAS20Initialized(address)"),
            "isCAS20Initialized(address)"
        );
        assert_eq!(SEL_IS_PAUSED, sel("isPaused(uint8)"), "isPaused(uint8)");
        assert_eq!(SEL_MINT, sel("mint(address,uint256)"), "mint(address,uint256)");
        assert_eq!(
            SEL_MINT_WITH_MEMO,
            sel("mintWithMemo(address,uint256,bytes32)"),
            "mintWithMemo(address,uint256,bytes32)"
        );
        assert_eq!(SEL_MULTIPLIER, sel("multiplier()"), "multiplier()");
        assert_eq!(SEL_NAME, sel("name()"), "name()");
        assert_eq!(SEL_NEW_UI_MULTIPLIER, sel("newUIMultiplier()"), "newUIMultiplier()");
        assert_eq!(SEL_NONCES, sel("nonces(address)"), "nonces(address)");
        assert_eq!(SEL_PAUSE, sel("pause(uint8[])"), "pause(uint8[])");
        assert_eq!(SEL_PAUSED_FEATURES, sel("pausedFeatures()"), "pausedFeatures()");
        assert_eq!(
            SEL_PENDING_POLICY_ADMIN,
            sel("pendingPolicyAdmin(uint64)"),
            "pendingPolicyAdmin(uint64)"
        );
        assert_eq!(
            SEL_PERMIT,
            sel("permit(address,address,uint256,uint256,uint8,bytes32,bytes32)"),
            "permit(address,address,uint256,uint256,uint8,bytes32,bytes32)"
        );
        assert_eq!(SEL_POLICY_ADMIN, sel("policyAdmin(uint64)"), "policyAdmin(uint64)");
        assert_eq!(SEL_POLICY_EXISTS, sel("policyExists(uint64)"), "policyExists(uint64)");
        assert_eq!(SEL_POLICY_ID, sel("policyId(bytes32)"), "policyId(bytes32)");
        assert_eq!(SEL_RENOUNCE_ADMIN, sel("renounceAdmin(uint64)"), "renounceAdmin(uint64)");
        assert_eq!(SEL_RENOUNCE_LAST_ADMIN, sel("renounceLastAdmin()"), "renounceLastAdmin()");
        assert_eq!(
            SEL_RENOUNCE_ROLE,
            sel("renounceRole(bytes32,address)"),
            "renounceRole(bytes32,address)"
        );
        assert_eq!(
            SEL_REVOKE_ROLE,
            sel("revokeRole(bytes32,address)"),
            "revokeRole(bytes32,address)"
        );
        assert_eq!(
            SEL_SCALED_BALANCE_OF,
            sel("scaledBalanceOf(address)"),
            "scaledBalanceOf(address)"
        );
        assert_eq!(
            SEL_SEIZE_WITH_MEMO,
            sel("seizeWithMemo(address,address,uint256,bytes32)"),
            "seizeWithMemo(address,address,uint256,bytes32)"
        );
        assert_eq!(
            SEL_SET_ROLE_ADMIN,
            sel("setRoleAdmin(bytes32,bytes32)"),
            "setRoleAdmin(bytes32,bytes32)"
        );
        assert_eq!(
            SEL_STAGE_UPDATE_ADMIN,
            sel("stageUpdateAdmin(uint64,address)"),
            "stageUpdateAdmin(uint64,address)"
        );
        assert_eq!(SEL_SUPPLY_CAP, sel("supplyCap()"), "supplyCap()");
        assert_eq!(
            SEL_SUPPORTS_INTERFACE,
            sel("supportsInterface(bytes4)"),
            "supportsInterface(bytes4)"
        );
        assert_eq!(SEL_SYMBOL, sel("symbol()"), "symbol()");
        assert_eq!(SEL_TO_RAW_BALANCE, sel("toRawBalance(uint256)"), "toRawBalance(uint256)");
        assert_eq!(
            SEL_TO_SCALED_BALANCE,
            sel("toScaledBalance(uint256)"),
            "toScaledBalance(uint256)"
        );
        assert_eq!(SEL_TO_UI_AMOUNT, sel("toUIAmount(uint256)"), "toUIAmount(uint256)");
        assert_eq!(SEL_TOTAL_SUPPLY, sel("totalSupply()"), "totalSupply()");
        assert_eq!(SEL_TOTAL_SUPPLY_UI, sel("totalSupplyUI()"), "totalSupplyUI()");
        assert_eq!(SEL_TRANSFER, sel("transfer(address,uint256)"), "transfer(address,uint256)");
        assert_eq!(
            SEL_TRANSFER_FROM,
            sel("transferFrom(address,address,uint256)"),
            "transferFrom(address,address,uint256)"
        );
        assert_eq!(
            SEL_TRANSFER_FROM_WITH_MEMO,
            sel("transferFromWithMemo(address,address,uint256,bytes32)"),
            "transferFromWithMemo(address,address,uint256,bytes32)"
        );
        assert_eq!(
            SEL_TRANSFER_FROM_WITH_MEMO_FORMAT,
            sel("transferFromWithMemoFormat(address,address,uint256,bytes32,bytes32)"),
            "transferFromWithMemoFormat(address,address,uint256,bytes32,bytes32)"
        );
        assert_eq!(
            SEL_TRANSFER_WITH_MEMO,
            sel("transferWithMemo(address,uint256,bytes32)"),
            "transferWithMemo(address,uint256,bytes32)"
        );
        assert_eq!(
            SEL_TRANSFER_WITH_MEMO_FORMAT,
            sel("transferWithMemoFormat(address,uint256,bytes32,bytes32)"),
            "transferWithMemoFormat(address,uint256,bytes32,bytes32)"
        );
        assert_eq!(SEL_UI_MULTIPLIER, sel("uiMultiplier()"), "uiMultiplier()");
        assert_eq!(SEL_UNPAUSE, sel("unpause(uint8[])"), "unpause(uint8[])");
        assert_eq!(
            SEL_UPDATE_ALLOWLIST,
            sel("updateAllowlist(uint64,bool,address[])"),
            "updateAllowlist(uint64,bool,address[])"
        );
        assert_eq!(
            SEL_UPDATE_BLOCKLIST,
            sel("updateBlocklist(uint64,bool,address[])"),
            "updateBlocklist(uint64,bool,address[])"
        );
        assert_eq!(
            SEL_UPDATE_COMPOSITE,
            sel("updateComposite(uint64,uint64[])"),
            "updateComposite(uint64,uint64[])"
        );
        assert_eq!(
            SEL_UPDATE_CONTRACT_URI,
            sel("updateContractURI(string)"),
            "updateContractURI(string)"
        );
        assert_eq!(
            SEL_UPDATE_EXTRA_METADATA,
            sel("updateExtraMetadata(string,string)"),
            "updateExtraMetadata(string,string)"
        );
        assert_eq!(
            SEL_UPDATE_MULTIPLIER,
            sel("updateMultiplier(uint256)"),
            "updateMultiplier(uint256)"
        );
        assert_eq!(SEL_UPDATE_NAME, sel("updateName(string)"), "updateName(string)");
        assert_eq!(SEL_UPDATE_PARAM, sel("updateParam(string,bytes)"), "updateParam(string,bytes)");
        assert_eq!(
            SEL_UPDATE_POLICY,
            sel("updatePolicy(bytes32,uint64)"),
            "updatePolicy(bytes32,uint64)"
        );
        assert_eq!(
            SEL_UPDATE_SUPPLY_CAP,
            sel("updateSupplyCap(uint256)"),
            "updateSupplyCap(uint256)"
        );
        assert_eq!(SEL_UPDATE_SYMBOL, sel("updateSymbol(string)"), "updateSymbol(string)");
        assert_eq!(
            SEL_UPDATE_UI_MULTIPLIER,
            sel("updateUIMultiplier(uint256,uint256)"),
            "updateUIMultiplier(uint256,uint256)"
        );
        assert_eq!(SEL_VARIANT_OF, sel("variantOf(address)"), "variantOf(address)");
        assert_eq!(
            ERR_AC_BAD_CONFIRMATION,
            sel("AccessControlBadConfirmation()"),
            "AccessControlBadConfirmation()"
        );
        assert_eq!(
            ERR_AC_UNAUTHORIZED,
            sel("AccessControlUnauthorizedAccount(address,bytes32)"),
            "AccessControlUnauthorizedAccount(address,bytes32)"
        );
        assert_eq!(
            ERR_ACCOUNT_NOT_SEIZABLE,
            sel("AccountNotSeizable(address)"),
            "AccountNotSeizable(address)"
        );
        assert_eq!(
            ERR_ALREADY_ACTIVATED,
            sel("AlreadyActivated(bytes32)"),
            "AlreadyActivated(bytes32)"
        );
        assert_eq!(
            ERR_ANNOUNCEMENT_ID_ALREADY_USED,
            sel("AnnouncementIdAlreadyUsed(string)"),
            "AnnouncementIdAlreadyUsed(string)"
        );
        assert_eq!(
            ERR_ANNOUNCEMENT_IN_PROGRESS,
            sel("AnnouncementInProgress()"),
            "AnnouncementInProgress()"
        );
        assert_eq!(
            ERR_BATCH_SIZE_TOO_LARGE,
            sel("BatchSizeTooLarge(uint256)"),
            "BatchSizeTooLarge(uint256)"
        );
        assert_eq!(
            ERR_CHILD_POLICIES_OUTSIDE_OF_RANGE,
            sel("ChildPoliciesOutsideOfRange()"),
            "ChildPoliciesOutsideOfRange()"
        );
        assert_eq!(ERR_CONTRACT_PAUSED, sel("ContractPaused(uint8)"), "ContractPaused(uint8)");
        assert_eq!(
            ERR_DELEGATE_CALL_NOT_ALLOWED,
            sel("DelegateCallNotAllowed()"),
            "DelegateCallNotAllowed()"
        );
        assert_eq!(
            ERR_EFFECTIVE_AT_IN_PAST,
            sel("EffectiveAtInPast(uint256)"),
            "EffectiveAtInPast(uint256)"
        );
        assert_eq!(
            ERR_EFFECTIVE_AT_TOO_FAR,
            sel("EffectiveAtTooFar(uint256)"),
            "EffectiveAtTooFar(uint256)"
        );
        assert_eq!(ERR_EMPTY_BATCH, sel("EmptyBatch()"), "EmptyBatch()");
        assert_eq!(ERR_EMPTY_FEATURE_SET, sel("EmptyFeatureSet()"), "EmptyFeatureSet()");
        assert_eq!(
            ERR_EXPIRED_SIGNATURE,
            sel("ExpiredSignature(uint256)"),
            "ExpiredSignature(uint256)"
        );
        assert_eq!(
            ERR_FEATURE_NOT_ACTIVATED,
            sel("FeatureNotActivated(bytes32)"),
            "FeatureNotActivated(bytes32)"
        );
        assert_eq!(
            ERR_INCOMPATIBLE_POLICY_TYPE,
            sel("IncompatiblePolicyType()"),
            "IncompatiblePolicyType()"
        );
        assert_eq!(ERR_INIT_CALL_FAILED, sel("InitCallFailed(uint256)"), "InitCallFailed(uint256)");
        assert_eq!(
            ERR_INSUFFICIENT_ALLOWANCE,
            sel("InsufficientAllowance(address,uint256,uint256)"),
            "InsufficientAllowance(address,uint256,uint256)"
        );
        assert_eq!(
            ERR_INSUFFICIENT_BALANCE,
            sel("InsufficientBalance(address,uint256,uint256)"),
            "InsufficientBalance(address,uint256,uint256)"
        );
        assert_eq!(
            ERR_INTERNAL_CALL_FAILED,
            sel("InternalCallFailed(bytes)"),
            "InternalCallFailed(bytes)"
        );
        assert_eq!(
            ERR_INTERNAL_CALL_MALFORMED,
            sel("InternalCallMalformed(bytes)"),
            "InternalCallMalformed(bytes)"
        );
        assert_eq!(
            ERR_INVALID_APPROVER,
            sel("InvalidApprover(address)"),
            "InvalidApprover(address)"
        );
        assert_eq!(
            ERR_INVALID_CHILD_POLICY,
            sel("InvalidChildPolicy(uint64)"),
            "InvalidChildPolicy(uint64)"
        );
        assert_eq!(ERR_INVALID_CURRENCY, sel("InvalidCurrency(string)"), "InvalidCurrency(string)");
        assert_eq!(ERR_INVALID_DECIMALS, sel("InvalidDecimals(uint8)"), "InvalidDecimals(uint8)");
        assert_eq!(ERR_INVALID_FORMAT_ID, sel("InvalidFormatId()"), "InvalidFormatId()");
        assert_eq!(ERR_INVALID_METADATA_KEY, sel("InvalidMetadataKey()"), "InvalidMetadataKey()");
        assert_eq!(ERR_INVALID_MULTIPLIER, sel("InvalidMultiplier()"), "InvalidMultiplier()");
        assert_eq!(
            ERR_INVALID_RECEIVER,
            sel("InvalidReceiver(address)"),
            "InvalidReceiver(address)"
        );
        assert_eq!(ERR_INVALID_SENDER, sel("InvalidSender(address)"), "InvalidSender(address)");
        assert_eq!(
            ERR_INVALID_SIGNER,
            sel("InvalidSigner(address,address)"),
            "InvalidSigner(address,address)"
        );
        assert_eq!(ERR_INVALID_SPENDER, sel("InvalidSpender(address)"), "InvalidSpender(address)");
        assert_eq!(
            ERR_INVALID_SUPPLY_CAP,
            sel("InvalidSupplyCap(uint256,uint256)"),
            "InvalidSupplyCap(uint256,uint256)"
        );
        assert_eq!(
            ERR_INVALID_VALUE,
            sel("InvalidValue(string,bytes)"),
            "InvalidValue(string,bytes)"
        );
        assert_eq!(ERR_INVALID_VARIANT, sel("InvalidVariant()"), "InvalidVariant()");
        assert_eq!(
            ERR_LAST_ADMIN_CANNOT_RENOUNCE,
            sel("LastAdminCannotRenounce()"),
            "LastAdminCannotRenounce()"
        );
        assert_eq!(
            ERR_LENGTH_MISMATCH,
            sel("LengthMismatch(uint256,uint256)"),
            "LengthMismatch(uint256,uint256)"
        );
        assert_eq!(
            ERR_MISSING_REQUIRED_FIELD,
            sel("MissingRequiredField(string)"),
            "MissingRequiredField(string)"
        );
        assert_eq!(ERR_NO_PENDING_ADMIN, sel("NoPendingAdmin()"), "NoPendingAdmin()");
        assert_eq!(ERR_NON_PAYABLE, sel("NonPayable()"), "NonPayable()");
        assert_eq!(ERR_NOT_SOLE_ADMIN, sel("NotSoleAdmin()"), "NotSoleAdmin()");
        assert_eq!(ERR_PANIC, sel("Panic(uint256)"), "Panic(uint256)");
        assert_eq!(
            ERR_POLICY_FORBIDS,
            sel("PolicyForbids(bytes32,uint64)"),
            "PolicyForbids(bytes32,uint64)"
        );
        assert_eq!(ERR_POLICY_NOT_FOUND, sel("PolicyNotFound()"), "PolicyNotFound()");
        assert_eq!(
            ERR_POLICY_NOT_FOUND_ID,
            sel("PolicyNotFound(uint64)"),
            "PolicyNotFound(uint64)"
        );
        assert_eq!(
            ERR_STATIC_CALL_NOT_ALLOWED,
            sel("StaticCallNotAllowed()"),
            "StaticCallNotAllowed()"
        );
        assert_eq!(
            ERR_SUPPLY_CAP_EXCEEDED,
            sel("SupplyCapExceeded(uint256,uint256)"),
            "SupplyCapExceeded(uint256,uint256)"
        );
        assert_eq!(
            ERR_TOKEN_ALREADY_EXISTS,
            sel("TokenAlreadyExists(address)"),
            "TokenAlreadyExists(address)"
        );
        assert_eq!(
            ERR_UI_MUL_MISSING,
            sel("UIMultiplierUpdateDoesNotExist()"),
            "UIMultiplierUpdateDoesNotExist()"
        );
        assert_eq!(
            ERR_UI_MUL_EXISTS,
            sel("UIMultiplierUpdateExists(uint256)"),
            "UIMultiplierUpdateExists(uint256)"
        );
        assert_eq!(ERR_UNAUTHORIZED, sel("Unauthorized()"), "Unauthorized()");
        assert_eq!(ERR_UNAUTHORIZED_ADDR, sel("Unauthorized(address)"), "Unauthorized(address)");
        assert_eq!(
            ERR_UNKNOWN_PARAM,
            sel("UnknownParam(string,bytes)"),
            "UnknownParam(string,bytes)"
        );
        assert_eq!(
            ERR_UNSUPPORTED_SCOPE,
            sel("UnsupportedPolicyType(bytes32)"),
            "UnsupportedPolicyType(bytes32)"
        );
        assert_eq!(
            ERR_UNSUPPORTED_VERSION,
            sel("UnsupportedVersion(uint8,uint8)"),
            "UnsupportedVersion(uint8,uint8)"
        );
        assert_eq!(ERR_ZERO_ADDRESS, sel("ZeroAddress()"), "ZeroAddress()");
    }

    #[test]
    fn topics_match_their_signatures() {
        assert_eq!(
            TOPIC_TRANSFER,
            keccak256("Transfer(address,address,uint256)"),
            "Transfer(address,address,uint256)"
        );
        assert_eq!(
            TOPIC_APPROVAL,
            keccak256("Approval(address,address,uint256)"),
            "Approval(address,address,uint256)"
        );
        assert_eq!(
            TOPIC_ROLE_GRANTED,
            keccak256("RoleGranted(bytes32,address,address)"),
            "RoleGranted(bytes32,address,address)"
        );
        assert_eq!(
            TOPIC_ROLE_REVOKED,
            keccak256("RoleRevoked(bytes32,address,address)"),
            "RoleRevoked(bytes32,address,address)"
        );
        assert_eq!(
            TOPIC_ROLE_ADMIN_CHANGED,
            keccak256("RoleAdminChanged(bytes32,bytes32,bytes32)"),
            "RoleAdminChanged(bytes32,bytes32,bytes32)"
        );
        assert_eq!(
            TOPIC_SEIZED,
            keccak256("Seized(address,address,address,uint256)"),
            "Seized(address,address,address,uint256)"
        );
        assert_eq!(
            TOPIC_LAST_ADMIN_RENOUNCED,
            keccak256("LastAdminRenounced(address)"),
            "LastAdminRenounced(address)"
        );
        assert_eq!(
            TOPIC_POLICY_UPDATED,
            keccak256("PolicyUpdated(bytes32,uint64,uint64)"),
            "PolicyUpdated(bytes32,uint64,uint64)"
        );
        assert_eq!(TOPIC_PAUSED, keccak256("Paused(address,uint8[])"), "Paused(address,uint8[])");
        assert_eq!(
            TOPIC_UNPAUSED,
            keccak256("Unpaused(address,uint8[])"),
            "Unpaused(address,uint8[])"
        );
        assert_eq!(
            TOPIC_SUPPLY_CAP_UPDATED,
            keccak256("SupplyCapUpdated(address,uint256,uint256)"),
            "SupplyCapUpdated(address,uint256,uint256)"
        );
        assert_eq!(
            TOPIC_NAME_UPDATED,
            keccak256("NameUpdated(address,string)"),
            "NameUpdated(address,string)"
        );
        assert_eq!(
            TOPIC_SYMBOL_UPDATED,
            keccak256("SymbolUpdated(address,string)"),
            "SymbolUpdated(address,string)"
        );
        assert_eq!(
            TOPIC_CONTRACT_URI_UPDATED,
            keccak256("ContractURIUpdated()"),
            "ContractURIUpdated()"
        );
        assert_eq!(
            TOPIC_EIP712_DOMAIN_CHANGED,
            keccak256("EIP712DomainChanged()"),
            "EIP712DomainChanged()"
        );
        assert_eq!(TOPIC_MEMO, keccak256("Memo(address,bytes32)"), "Memo(address,bytes32)");
        assert_eq!(
            TOPIC_MEMO_FORMAT_DECLARED,
            keccak256("MemoFormatDeclared(address,bytes32,bytes32)"),
            "MemoFormatDeclared(address,bytes32,bytes32)"
        );
        assert_eq!(
            TOPIC_MULTIPLIER_UPDATED,
            keccak256("MultiplierUpdated(uint256)"),
            "MultiplierUpdated(uint256)"
        );
        assert_eq!(
            TOPIC_ANNOUNCEMENT,
            keccak256("Announcement(address,string,string,string)"),
            "Announcement(address,string,string,string)"
        );
        assert_eq!(
            TOPIC_END_ANNOUNCEMENT,
            keccak256("EndAnnouncement(string)"),
            "EndAnnouncement(string)"
        );
        assert_eq!(
            TOPIC_EXTRA_METADATA_UPDATED,
            keccak256("ExtraMetadataUpdated(string,string)"),
            "ExtraMetadataUpdated(string,string)"
        );
        assert_eq!(
            TOPIC_UI_MULTIPLIER_UPDATED,
            keccak256("UIMultiplierUpdated(uint256,uint256,uint256)"),
            "UIMultiplierUpdated(uint256,uint256,uint256)"
        );
        assert_eq!(
            TOPIC_UI_MULTIPLIER_UPDATE_CANCELLED,
            keccak256("UIMultiplierUpdateCancelled(uint256,uint256)"),
            "UIMultiplierUpdateCancelled(uint256,uint256)"
        );
        assert_eq!(
            TOPIC_POLICY_CREATED,
            keccak256("PolicyCreated(uint64,address,uint8)"),
            "PolicyCreated(uint64,address,uint8)"
        );
        assert_eq!(
            TOPIC_POLICY_ADMIN_STAGED,
            keccak256("PolicyAdminStaged(uint64,address,address)"),
            "PolicyAdminStaged(uint64,address,address)"
        );
        assert_eq!(
            TOPIC_POLICY_ADMIN_UPDATED,
            keccak256("PolicyAdminUpdated(uint64,address,address)"),
            "PolicyAdminUpdated(uint64,address,address)"
        );
        assert_eq!(
            TOPIC_COMPOSITE_POLICY_UPDATED,
            keccak256("CompositePolicyUpdated(uint64,address,uint64[])"),
            "CompositePolicyUpdated(uint64,address,uint64[])"
        );
        assert_eq!(
            TOPIC_ALLOWLIST_UPDATED,
            keccak256("AllowlistUpdated(uint64,address,bool,address[])"),
            "AllowlistUpdated(uint64,address,bool,address[])"
        );
        assert_eq!(
            TOPIC_BLOCKLIST_UPDATED,
            keccak256("BlocklistUpdated(uint64,address,bool,address[])"),
            "BlocklistUpdated(uint64,address,bool,address[])"
        );
        assert_eq!(
            TOPIC_CAS20_CREATED,
            keccak256("CAS20Created(address,uint8,string,string,uint8,bytes)"),
            "CAS20Created(address,uint8,string,string,uint8,bytes)"
        );
        assert_eq!(
            TOPIC_FEATURE_ACTIVATED,
            keccak256("FeatureActivated(bytes32,address)"),
            "FeatureActivated(bytes32,address)"
        );
        assert_eq!(
            TOPIC_FEATURE_DEACTIVATED,
            keccak256("FeatureDeactivated(bytes32,address)"),
            "FeatureDeactivated(bytes32,address)"
        );
        assert_eq!(
            TOPIC_ADMIN_CHANGED,
            keccak256("AdminChanged(address,address,address)"),
            "AdminChanged(address,address,address)"
        );
        assert_eq!(
            TOPIC_PARAM_CHANGE,
            keccak256("ParamChange(string,bytes)"),
            "ParamChange(string,bytes)"
        );
    }

    #[test]
    fn words_match_their_preimages() {
        assert_eq!(ROLE_MINT, keccak256("MINT_ROLE"), "ROLE_MINT");
        assert_eq!(ROLE_BURN, keccak256("BURN_ROLE"), "ROLE_BURN");
        assert_eq!(ROLE_SEIZE, keccak256("SEIZE_ROLE"), "ROLE_SEIZE");
        assert_eq!(ROLE_PAUSE, keccak256("PAUSE_ROLE"), "ROLE_PAUSE");
        assert_eq!(ROLE_UNPAUSE, keccak256("UNPAUSE_ROLE"), "ROLE_UNPAUSE");
        assert_eq!(ROLE_METADATA, keccak256("METADATA_ROLE"), "ROLE_METADATA");
        assert_eq!(ROLE_OPERATOR, keccak256("OPERATOR_ROLE"), "ROLE_OPERATOR");
        assert_eq!(
            SCOPE_TRANSFER_SENDER,
            keccak256("TRANSFER_SENDER_POLICY"),
            "SCOPE_TRANSFER_SENDER"
        );
        assert_eq!(
            SCOPE_TRANSFER_RECEIVER,
            keccak256("TRANSFER_RECEIVER_POLICY"),
            "SCOPE_TRANSFER_RECEIVER"
        );
        assert_eq!(
            SCOPE_TRANSFER_EXECUTOR,
            keccak256("TRANSFER_EXECUTOR_POLICY"),
            "SCOPE_TRANSFER_EXECUTOR"
        );
        assert_eq!(SCOPE_MINT_RECEIVER, keccak256("MINT_RECEIVER_POLICY"), "SCOPE_MINT_RECEIVER");
        assert_eq!(SCOPE_SEIZE_EXEMPT, keccak256("SEIZE_EXEMPT_POLICY"), "SCOPE_SEIZE_EXEMPT");
        assert_eq!(
            SCOPE_SEIZE_RECEIVER,
            keccak256("SEIZE_RECEIVER_POLICY"),
            "SCOPE_SEIZE_RECEIVER"
        );
        assert_eq!(FEATURE_ASSET, keccak256("bsc.cas20_asset"), "FEATURE_ASSET");
        assert_eq!(FEATURE_STABLECOIN, keccak256("bsc.cas20_stablecoin"), "FEATURE_STABLECOIN");
        assert_eq!(
            FEATURE_POLICY_REGISTRY,
            keccak256("bsc.policy_registry"),
            "FEATURE_POLICY_REGISTRY"
        );
        assert_eq!(DOMAIN_TYPEHASH, keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"), "DOMAIN_TYPEHASH");
        assert_eq!(PERMIT_TYPEHASH, keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"), "PERMIT_TYPEHASH");
        assert_eq!(MARKER_CODE_HASH, keccak256([0xef]));
    }

    #[test]
    fn roots_match_their_namespaces() {
        assert_eq!(ROOT_CORE, erc7201_root("bsc.cas20"), "bsc.cas20");
        assert_eq!(ROOT_ASSET, erc7201_root("bsc.cas20.asset"), "bsc.cas20.asset");
        assert_eq!(ROOT_STABLECOIN, erc7201_root("bsc.cas20.stablecoin"), "bsc.cas20.stablecoin");
        assert_eq!(ROOT_POLICY, erc7201_root("bsc.policy_registry"), "bsc.policy_registry");
        assert_eq!(
            ROOT_ACTIVATION,
            erc7201_root("bsc.activation_registry"),
            "bsc.activation_registry"
        );
    }
}
