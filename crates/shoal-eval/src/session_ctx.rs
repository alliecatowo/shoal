//! Session identity, authority, and presentation state.
//!
//! Keeping these fields in one typed value makes them mandatory at the child
//! construction boundary. A child inherits identity, policy, and echo policy,
//! while terminal ownership and root-only mutable handles are cleared in one
//! place.

use super::*;

pub(crate) struct SessionCtx {
    pub(crate) principal: String,
    pub(crate) session_id: String,
    pub(crate) leash: Option<(LeashPolicy, String)>,
    pub(crate) echo_mode: EchoMode,
    pub(crate) interactive: bool,
    pub(crate) journal: Option<Journal>,
    pub(crate) sink: Option<StatementSink>,
}

impl Default for SessionCtx {
    fn default() -> Self {
        Self {
            principal: "human".into(),
            session_id: "default".into(),
            leash: None,
            echo_mode: EchoMode::default(),
            interactive: false,
            journal: None,
            sink: None,
        }
    }
}

impl SessionCtx {
    /// The complete session value a child must receive. Identity, authority,
    /// and presentation policy are inherited together; root-only mutable
    /// handles and terminal ownership are deliberately cleared.
    pub(crate) fn for_child(&self) -> Self {
        Self {
            principal: self.principal.clone(),
            session_id: self.session_id.clone(),
            leash: self.leash.clone(),
            echo_mode: self.echo_mode,
            interactive: false,
            journal: None,
            sink: None,
        }
    }
}
