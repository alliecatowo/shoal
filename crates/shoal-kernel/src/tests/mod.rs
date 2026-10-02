//! Kernel unit tests grouped by the subsystem contract they exercise.

use super::*;

mod support;
use support::*;

mod authentication;
mod authority_enforcement;
mod connections_sessions;
mod events_persistence;
mod exec_plans_tasks;
mod language_outcomes;
mod rendering;
mod resource_lifecycle;
mod task_control;
mod token_admin;
mod transport;
mod values_cas;
