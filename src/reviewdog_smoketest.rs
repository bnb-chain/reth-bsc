//! Throwaway module used only to verify that the clippy incremental gate
//! (reviewdog) correctly reports issues introduced on this PR's diff lines.
//! Safe to delete once the mechanism has been validated.

pub fn smoketest() {
    let unused = 1;
    println!("reviewdog smoketest");
}
