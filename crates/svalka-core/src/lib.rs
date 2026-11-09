//! The parts of svalka that touch no hardware: curves, config, the control
//! logic, the state snapshot, the history format and the PWM scale. Shared by
//! the daemon and, later, the TUI.

pub mod config;
pub mod controller;
pub mod curve;
pub mod duration;
pub mod history;
pub mod pwm;
pub mod state;
