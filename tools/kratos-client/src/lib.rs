//! The measuring and comparing tools behind the `kratos-client` command: the post and turn latency
//! benchmarks and the output normalizer. The engine's tests run the benchmarks too. The engine
//! itself never links this crate, because the turn benchmark steers `fake-codex`.

mod bench;
mod bench_turns;
mod normalize;

use kratos_api_client::ClientError;

pub use bench::{BenchReport, bench_post};
pub use bench_turns::{TurnBenchReport, bench_turns};
pub use normalize::Normalizer;

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("{0}")]
    Failed(String),
}

/// A local failure, such as an unreadable control file, reads as the client's I/O error did.
impl From<std::io::Error> for BenchError {
    fn from(error: std::io::Error) -> BenchError {
        BenchError::Client(ClientError::Io(error))
    }
}
