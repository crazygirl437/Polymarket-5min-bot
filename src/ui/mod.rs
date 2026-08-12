//! Full-screen terminal dashboard for the arbitrage bot.

mod render;
mod runner;
mod state;

pub use render::draw;
pub use runner::spawn_dashboard_thread;
pub use state::{
    decimal_to_f64, symbol_short, DashboardAction, DashboardHandle, DashboardState,
};
