//! Dependency validator for archdots profiles.
//!
//! The central type is [`Validator`], which cross-references a profile's
//! declared dependencies with what is installed via [`crate::packages::PackageDB`]
//! and infers additional dependencies from `[[files]]` config entries.
//!
//! # Example (sketch — requires a real `PackageDB`)
//!
//! ```no_run
//! use std::path::Path;
//! use archdots_core::{packages::PackageDB, profile::Profile, validator::{Validator, ValidatorOptions}};
//!
//! let db = PackageDB::new().unwrap();
//! let home = Path::new("/home/alice");
//! let validator = Validator::new(&db, home);
//! let profile = Profile::load_from_file(Path::new("/cfg/archdots/profiles/rice.toml")).unwrap();
//! let report = validator.validate(&profile, ValidatorOptions::default()).unwrap();
//! println!("exit code: {}", report.exit_code());
//! ```

mod engine;
mod types;

pub use engine::Validator;
pub use types::{
    DepEntry, DepKind, DepSource, DepStatus, LocatedMention, ValidationReport, ValidationWarning,
    ValidatorError, ValidatorOptions,
};
