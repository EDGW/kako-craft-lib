//! Crate-level tests for opening containers and shared logging behavior.

use std::sync::Once;

use tracing_subscriber::layer::SubscriberExt;

static INIT_TRACING: Once = Once::new();

pub(crate) fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_test_writer());

        tracing::subscriber::set_global_default(subscriber)
            .expect("failed to initialize tracing for tests");
    });
}
