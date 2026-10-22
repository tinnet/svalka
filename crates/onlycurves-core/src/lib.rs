//! The parts of onlycurves that touch no hardware: curves, config and the PWM
//! scale. Shared by the daemon and, later, the TUI.

pub mod config;
pub mod curve;
pub mod duration;
pub mod pwm;
