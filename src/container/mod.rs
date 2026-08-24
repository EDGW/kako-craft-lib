macro_rules! container_operation_span {
    ($logger:expr, $operation:literal) => {
        tracing::debug_span!(
            "container_operation",
            logger = %$logger.name(),
            container_uid = %$logger.uid(),
            container_kind = $logger.kind(),
            operation = $operation,
        )
    };
}

mod check;
mod error;
pub mod link;
pub mod local;
mod model;
mod open;
mod traits;
mod validation;

pub(crate) use check::{apply_validation_check_action, validation_check_issues};
pub use error::*;
pub use link::{LinkContainer, LinkContainerWriteGuard};
pub use local::LocalContainer;
pub use model::*;
pub use open::*;
pub use traits::*;

#[cfg(test)]
mod tests;
