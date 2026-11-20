fn main() {
    #[cfg(feature = "circl-backend")]
    build_circl();
    println!("cargo:rerun-if-changed=native/verify.c");
    #[cfg(feature = "openssl-backend")]
    {
        let library = pkg_config::Config::new()
            .atleast_version("3.5.0")
            .cargo_metadata(false)
            .probe("libcrypto")
            .expect("openssl-backend requires OpenSSL >= 3.5; set PKG_CONFIG_PATH to its lib/pkgconfig directory");
        cc::Build::new()
            .file("native/verify.c")
            .includes(&library.include_paths)
            .warnings_into_errors(true)
            .compile("bsc_pqc_openssl");
        // Emit libcrypto after the shim for linkers that resolve archives in order.
        pkg_config::Config::new().atleast_version("3.5.0").probe("libcrypto").unwrap();
    }
}

#[cfg(feature = "circl-backend")]
fn build_circl() {
    use std::{env, path::PathBuf, process::Command};

    for file in ["native/circl/main.go", "native/circl/go.mod", "native/circl/go.sum"] {
        println!("cargo:rerun-if-changed={file}");
    }
    for var in ["GO", "CC", "CGO_CFLAGS", "CGO_LDFLAGS", "CARGO_NET_OFFLINE"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let target = env::var("TARGET").unwrap();
    assert_eq!(
        env::var("HOST").unwrap(),
        target,
        "circl-backend currently requires a native build"
    );
    assert!(
        matches!(
            target.as_str(),
            "aarch64-apple-darwin"
                | "x86_64-apple-darwin"
                | "aarch64-unknown-linux-gnu"
                | "x86_64-unknown-linux-gnu"
        ),
        "circl-backend currently supports macOS and Linux GNU on arm64/x86_64"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let go = env::var_os("GO").unwrap_or_else(|| "go".into());
    let version = Command::new(&go)
        .arg("version")
        .output()
        .expect("circl-backend requires Go >= 1.25 and a C compiler");
    assert!(version.status.success(), "go version failed");
    println!(
        "cargo:rustc-env=PQC_CIRCL_GO_VERSION={}",
        String::from_utf8(version.stdout).unwrap().trim()
    );
    let mut build = Command::new(go);
    build
        .current_dir("native/circl")
        .args([
            "build",
            "-mod=readonly",
            "-trimpath",
            "-buildvcs=false",
            "-buildmode=c-archive",
            "-o",
        ])
        .arg(out.join("libbsc_pqc_circl.a"))
        .arg(".")
        .env("CGO_ENABLED", "1")
        .env("GOOS", if target.contains("apple") { "darwin" } else { "linux" })
        .env("GOARCH", if target.starts_with("aarch64") { "arm64" } else { "amd64" })
        .env("GOTOOLCHAIN", "local")
        .env("GOWORK", "off")
        .env("GOCACHE", out.join("go-cache"));
    if env::var("CARGO_NET_OFFLINE").as_deref() == Ok("true") {
        build.env("GOPROXY", "off");
    }
    assert!(
        build.status().expect("failed to execute Go compiler").success(),
        "CIRCL C archive build failed"
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=bsc_pqc_circl");
    println!("cargo:rustc-link-lib=pthread");
    if target.contains("linux") {
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=m");
    }
}
