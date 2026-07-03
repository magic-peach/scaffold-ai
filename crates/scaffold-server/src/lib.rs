//! HTTP layer and worker for Scaffold AI. Exposed as a library so
//! integration tests can build the router and worker against fakes.

pub mod routes;
pub mod state;
pub mod worker;

pub use routes::build_router;
pub use state::AppState;
