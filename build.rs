use std::{collections::BTreeMap, fs, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=proto/mev.proto");
    let mut prost_config = tonic_prost_build::Config::new();
    prost_config.protoc_executable(
        protoc_bin_vendored::protoc_bin_path().expect("failed to locate vendored protoc binary"),
    );
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_with_config(prost_config, &["proto/mev.proto"], &["proto"])
        .expect("failed to compile MEV gRPC protobuf definitions");

    // Emit reth-bsc git SHA for startup version logging.
    let git_sha_short = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let git_sha_long = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    // Emit the BSC release identifier (e.g. `0.1.0-fix`) from the nearest git tag.
    //
    // `git describe --tags` resolves the closest ancestor tag, so a binary built
    // from the `v0.1.0-fix` tag reports `0.1.0-fix` (distinguishing it from
    // `v0.1.0`), while dev builds report `<tag>-<n>-g<sha>[-dirty]`. Falls back to
    // the Cargo package version when git metadata is unavailable.
    let bsc_version = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { String::from_utf8(o.stdout).ok() } else { None })
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=RETH_BSC_VERSION={bsc_version}");
    println!("cargo:rustc-env=RETH_BSC_GIT_SHA={git_sha_short}");
    println!("cargo:rustc-env=RETH_BSC_GIT_SHA_LONG={git_sha_long}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/refs/tags");

    // Define hardforks that contain system contracts (matching hardfork_to_dir_name function)
    let hardforks = vec![
        "bruno",
        "euler",
        "feynman",
        "feynman_fix",
        "gibbs",
        "kepler",
        "luban",
        "mirror_sync",
        "moran",
        "niels",
        "planck",
        "plato",
        "ramanujan",
        "haber_fix",
        "bohr",
        "pascal",
        "lorentz",
        "maxwell",
        "pasteur",
    ];

    // Rerun if any of the hardfork directories change
    for hardfork in &hardforks {
        println!("cargo:rerun-if-changed=src/system_contracts/{hardfork}");
    }

    let contracts_dir = "src/system_contracts";
    let mut contract_data: BTreeMap<String, String> = BTreeMap::new();

    // Get all hardfork directories
    if let Ok(entries) = fs::read_dir(contracts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let hardfork_name = path.file_name().unwrap().to_string_lossy();

                // Skip the abi.rs file and other non-hardfork directories
                if hardfork_name == "abi" || hardfork_name.starts_with('.') {
                    continue;
                }

                // Process mainnet, chapel and rialto subdirectories
                for network in ["mainnet", "chapel", "rialto"] {
                    let network_path = path.join(network);
                    if network_path.exists() {
                        if let Ok(contract_files) = fs::read_dir(&network_path) {
                            for contract_file in contract_files.flatten() {
                                let contract_path = contract_file.path();
                                if contract_path.is_file() {
                                    let contract_name =
                                        contract_path.file_name().unwrap().to_string_lossy();

                                    // Read the contract hex data
                                    if let Ok(hex_data) = fs::read_to_string(&contract_path) {
                                        let hex_data = hex_data.trim();

                                        // Store the contract data
                                        let key =
                                            format!("{hardfork_name}_{network}_{contract_name}");
                                        contract_data.insert(key, hex_data.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Generate the Rust code
    let mut code = String::new();
    code.push_str("// Auto-generated by build.rs - DO NOT EDIT\n");
    code.push_str("use phf::phf_map;\n\n");

    code.push_str(
        "pub static EMBEDDED_CONTRACTS: phf::Map<&'static str, &'static str> = phf_map! {\n",
    );

    for (key, hex_data) in &contract_data {
        code.push_str(&format!("    \"{key}\" => \"{hex_data}\",\n"));
    }

    code.push_str("};\n");

    // Write the generated code
    fs::write("src/system_contracts/embedded_contracts.rs", code).unwrap();
}
