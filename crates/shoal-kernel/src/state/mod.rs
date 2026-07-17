mod connections;
mod sessions;

pub(crate) use connections::{ConnectionPermit, ConnectionRegistry};
#[cfg(test)]
pub(crate) use sessions::MAX_SESSIONS_PER_PRINCIPAL;
pub(crate) use sessions::SessionRegistry;
