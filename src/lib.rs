#![cfg_attr(feature="clippy", feature(plugin))]
#![cfg_attr(feature="clippy", plugin(clippy))]

mod evaluator;
mod resolver;

pub use self::evaluator::*;
pub use self::resolver::*;
