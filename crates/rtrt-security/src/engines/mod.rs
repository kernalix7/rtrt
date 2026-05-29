//! Engine registry. Each submodule implements one [`Engine`]; `registry`
//! returns the full set the runner dispatches across.

pub mod ai;
pub mod deps;
pub mod licenses;
pub mod patterns;
pub mod secrets;

use crate::engine::Engine;

/// All built-in engines. The runner only invokes the ones a profile's rules
/// route to, so listing them all here is cheap.
pub fn registry() -> Vec<Box<dyn Engine>> {
    vec![
        Box::new(secrets::SecretsEngine),
        Box::new(licenses::LicensesEngine),
        Box::new(deps::DepsEngine),
        Box::new(patterns::PatternsEngine),
        Box::new(ai::AiEngine),
    ]
}
