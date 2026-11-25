//! Durable metadata catalog.

pub mod persistent;
pub(crate) mod worker;

use std::path::Path;
use std::sync::Arc;

/// Open and initialize the local durable catalog, including its schema.
pub fn open(path: &Path) -> Result<Arc<persistent::PersistentDb>, redb::Error> {
    let result = persistent::PersistentDb::open(path).map(Arc::new);
    if let Some(logger) = crate::telemetry::global() {
        let (severity, message) = if result.is_ok() {
            (crate::telemetry::Severity::Info, "metadata database opened")
        } else {
            (
                crate::telemetry::Severity::Error,
                "metadata database initialization failed",
            )
        };
        logger.emit(crate::telemetry::Event::new(severity, "database", message));
    }
    result
}
