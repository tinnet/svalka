//! The parts of onlycurves that touch no hardware: curves, config, the control
//! logic, the state snapshot and the PWM
//! scale. Shared by the daemon and, later, the TUI.

pub mod config;
pub mod controller;
pub mod curve;
pub mod duration;
pub mod pwm;
pub mod state;
