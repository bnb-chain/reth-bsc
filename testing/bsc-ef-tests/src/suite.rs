//! Test suite abstraction

use crate::{Case, Error};
use rayon::iter::{ParallelBridge, ParallelIterator};
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

/// A test suite.
pub trait Suite: Sized {
    /// The type of test case in this suite.
    type Case: Case + Send + Sync;

    /// The path to the suite.
    fn suite_path(&self) -> &Path;

    /// Run the test suite.
    fn run(&self) {
        self.run_with_filter(|_| true)
    }

    /// Run only tests matching the given filter.
    fn run_only(&self, filter: &str) {
        self.run_with_filter(|path| {
            path.to_string_lossy().contains(filter)
        })
    }

    /// Run tests with a custom filter.
    fn run_with_filter<F>(&self, filter: F)
    where
        F: Fn(&Path) -> bool + Send + Sync,
    {
        let suite_path = self.suite_path();
        if !suite_path.exists() {
            panic!(
                "Suite path does not exist: {}. Did you download the test fixtures?",
                suite_path.display()
            );
        }

        let results: Vec<_> = WalkDir::new(suite_path)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .filter(|e| filter(e.path()))
            .par_bridge()
            .map(|entry| run_case::<Self::Case>(&entry))
            .collect();

        let (passed, failed, skipped) = count_results(&results);

        println!("\nTest Results:");
        println!("  Passed:  {passed}");
        println!("  Failed:  {failed}");
        println!("  Skipped: {skipped}");

        if failed > 0 {
            println!("\nFailed tests:");
            for (path, err) in results
                .iter()
                .filter_map(|(path, result)| result.as_ref().err().map(|e| (path, e)))
                .filter(|(_, e)| !matches!(e, Error::Skipped))
            {
                println!("  {}: {err}", path.display());
            }
            panic!("{failed} tests failed");
        }
    }
}

fn run_case<T: Case>(entry: &DirEntry) -> (std::path::PathBuf, Result<(), Error>) {
    let path = entry.path().to_path_buf();
    let result = T::load(&path).and_then(|case| case.run());
    (path, result)
}

fn count_results(results: &[(std::path::PathBuf, Result<(), Error>)]) -> (usize, usize, usize) {
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;

    for (_, result) in results {
        match result {
            Ok(_) => passed += 1,
            Err(Error::Skipped) => skipped += 1,
            Err(_) => failed += 1,
        }
    }

    (passed, failed, skipped)
}
