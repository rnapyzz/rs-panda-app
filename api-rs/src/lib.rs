//! F-Panda の API サーバー（Go 版 api/ の Rust への移植）。

#[macro_use]
pub mod db;

pub mod audit;
pub mod auth;
pub mod codes;
pub mod config;
pub mod csvio;
pub mod error;
pub mod http;
pub mod jst;
pub mod master;
pub mod migrate;
pub mod password;
pub mod server;
pub mod state;
pub mod visibility;

pub use error::{AppError, AppResult};
pub use state::AppState;
