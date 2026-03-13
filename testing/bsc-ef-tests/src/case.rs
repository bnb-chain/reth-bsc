//! Test case abstractions

use crate::result::Error;
use std::path::Path;

/// A single test case.
pub trait Case: Sized {
    /// Load the test from the given path.
    fn load(path: &Path) -> Result<Self, Error>;

    /// Run the test.
    fn run(&self) -> Result<(), Error>;
}

/// A collection of test cases.
#[derive(Debug)]
pub struct Cases<T> {
    /// The test cases.
    pub tests: Vec<T>,
}

impl<T: Case> Cases<T> {
    /// Run all test cases.
    pub fn run(&self) -> Vec<Result<(), Error>> {
        self.tests.iter().map(|case| case.run()).collect()
    }
}
