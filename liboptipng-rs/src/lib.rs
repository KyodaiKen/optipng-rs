/// Core PNG optimization library — reduction, trials, IDAT fast path, external decoders.

pub mod models;
pub mod reducer;
pub mod trials;
pub mod idat_fast;
pub mod external;
pub mod utils;
pub mod pipeline;

pub use models::*;
pub use reducer::*;
pub use trials::*;
pub use idat_fast::*;
pub use external::*;
pub use utils::*;
pub use pipeline::run_optimization;
