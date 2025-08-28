use reth_discv4::NodeRecord;

pub fn bsc_mainnet_nodes() -> Vec<NodeRecord> {
    parse_nodes(BSC_MAINNET_BOOTNODES)
}

/// Returns parsed bsc mainnet nodes
pub fn bsc_testnet_nodes() -> Vec<NodeRecord> {
    parse_nodes(BSC_TESTNET_BOOTNODES)
}

/// Returns parsed bsc qanet nodes
pub fn bsc_qanet_nodes() -> Vec<NodeRecord> {
    parse_nodes(BSC_QANET_BOOTNODES)
}

/// Parses all the nodes
pub fn parse_nodes(nodes: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<NodeRecord> {
    nodes.into_iter().map(|s| s.as_ref().parse().unwrap()).collect()
}

/// Bsc mainnet boot nodes.
pub static BSC_MAINNET_BOOTNODES: &[&str] = &[
    "enode://433c8bfdf53a3e2268ccb1b829e47f629793291cbddf0c76ae626da802f90532251fc558e2e0d10d6725e759088439bf1cd4714716b03a259a35d4b2e4acfa7f@52.69.102.73:30311",
    "enode://571bee8fb902a625942f10a770ccf727ae2ba1bab2a2b64e121594a99c9437317f6166a395670a00b7d93647eacafe598b6bbcef15b40b6d1a10243865a3e80f@35.73.84.120:30311",
    "enode://fac42fb0ba082b7d1eebded216db42161163d42e4f52c9e47716946d64468a62da4ba0b1cac0df5e8bf1e5284861d757339751c33d51dfef318be5168803d0b5@18.203.152.54:30311",
    "enode://3063d1c9e1b824cfbb7c7b6abafa34faec6bb4e7e06941d218d760acdd7963b274278c5c3e63914bd6d1b58504c59ec5522c56f883baceb8538674b92da48a96@34.250.32.100:30311",
    "enode://ad78c64a4ade83692488aa42e4c94084516e555d3f340d9802c2bf106a3df8868bc46eae083d2de4018f40e8d9a9952c32a0943cd68855a9bc9fd07aac982a6d@34.204.214.24:30311",
    "enode://5db798deb67df75d073f8e2953dad283148133acb520625ea804c9c4ad09a35f13592a762d8f89056248f3889f6dcc33490c145774ea4ff2966982294909b37a@107.20.191.97:30311",
];

/// Bsc testnet boot nodes.
pub static BSC_TESTNET_BOOTNODES: &[&str] = &[
    "enode://0637d1e62026e0c8685b1db0ca1c767c78c95c3fab64abc468d1a64b12ca4b530b46b8f80c915aec96f74f7ffc5999e8ad6d1484476f420f0c10e3d42361914b@52.199.214.252:30311",
    "enode://df1e8eb59e42cad3c4551b2a53e31a7e55a2fdde1287babd1e94b0836550b489ba16c40932e4dacb16cba346bd442c432265a299c4aca63ee7bb0f832b9f45eb@52.51.80.128:30311",
    "enode://ecd664250ca19b1074dcfbfb48576a487cc18d052064222a363adacd2650f8e08fb3db9de7a7aecb48afa410eaeb3285e92e516ead01fb62598553aed91ee15e@3.209.122.123:30311",
    "enode://665cf77ca26a8421cfe61a52ac312958308d4912e78ce8e0f61d6902e4494d4cc38f9b0dd1b23a427a7a5734e27e5d9729231426b06bb9c73b56a142f83f6b68@52.72.123.113:30311",
];


/// Bsc qanet boot nodes.
pub static BSC_QANET_BOOTNODES: &[&str] = &[
    "enode://b78cba3067e3043e0d6b72931c29ae463c10533b149bdc23de54304cacf5f434e903ae2b8d4485f1ad103e6882301a77f03b679a51e169ab4afcab635cb614c2@10.179.43.231:30311",
    "enode://c1362b6d4a9693d9372c0c82f3186bfb9383a6ffe3e147507e5515474e61bf192cbf0599a7a00b878cc154582b96174cc6d53cccdcd88d110f721d6b30443388@10.179.41.29:30311",
    "enode://4c983187454c632312d35ccbbb5b801ec0081c202eb2fa4a506e218492f46312285c66aa7b470b43a1d0f90e1a1c7247e3ad2c6971ae8bcda9ad063f9c54af6b@10.179.41.103:30311"
];