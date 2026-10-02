use super::*;

pub(super) struct State {
    pub(super) limits: StoreLimits,
    pub(super) capabilities: Arc<dyn CapabilityProvider>,
    pub(super) hostcall_bytes: usize,
    pub(super) hostcall_remaining_bytes: usize,
    pub(super) hostcall_remaining_calls: usize,
    pub(super) declared_effects: Vec<Effect>,
}

impl abi::shoal::plugin::types::Host for State {}

impl abi::shoal::plugin::host::Host for State {
    fn now_ns(&mut self) -> Result<u64, GuestError> {
        self.begin_hostcall(0)?;
        self.charge_hostcall_output(std::mem::size_of::<u64>())?;
        let effect = Effect::Time;
        self.authorize(&effect)?;
        self.capabilities.now_ns().map_err(|error| {
            guest_error(
                ErrorKind::Internal,
                bounded_text(error.message, self.hostcall_bytes),
            )
        })
    }

    fn read_file(&mut self, path: String) -> Result<Vec<u8>, GuestError> {
        self.begin_hostcall(path.len())?;
        let path = PathBuf::from(path);
        let effect = Effect::FsRead {
            paths: vec![path.clone()],
        };
        self.authorize(&effect)?;
        let bytes = self
            .capabilities
            .read_file(
                &path,
                self.hostcall_bytes.min(self.hostcall_remaining_bytes),
            )
            .map_err(|error| {
                guest_error(
                    ErrorKind::Internal,
                    bounded_text(error.message, self.hostcall_bytes),
                )
            })?;
        if bytes.len() > self.hostcall_bytes {
            Err(guest_error(
                ErrorKind::ResourceLimit,
                format!(
                    "hostcall output exceeds the {}-byte limit",
                    self.hostcall_bytes
                ),
            ))
        } else {
            self.charge_hostcall_output(bytes.len())?;
            Ok(bytes)
        }
    }
}

impl State {
    fn begin_hostcall(&mut self, input_bytes: usize) -> Result<(), GuestError> {
        if self.hostcall_remaining_calls == 0 {
            return Err(guest_error(
                ErrorKind::ResourceLimit,
                "plugin hostcall count limit reached".into(),
            ));
        }
        if input_bytes > self.hostcall_bytes || input_bytes > self.hostcall_remaining_bytes {
            return Err(guest_error(
                ErrorKind::ResourceLimit,
                "plugin hostcall input exceeds the byte budget".into(),
            ));
        }
        self.hostcall_remaining_calls -= 1;
        self.hostcall_remaining_bytes -= input_bytes;
        Ok(())
    }

    fn charge_hostcall_output(&mut self, output_bytes: usize) -> Result<(), GuestError> {
        if output_bytes > self.hostcall_bytes || output_bytes > self.hostcall_remaining_bytes {
            return Err(guest_error(
                ErrorKind::ResourceLimit,
                "plugin hostcall output exceeds the byte budget".into(),
            ));
        }
        self.hostcall_remaining_bytes -= output_bytes;
        Ok(())
    }

    fn authorize(&self, effect: &Effect) -> Result<(), GuestError> {
        if !self
            .declared_effects
            .iter()
            .any(|declared| effect_covers(declared, effect))
        {
            return Err(guest_error(
                ErrorKind::PermissionDenied,
                format!("plugin attempted undeclared effect {effect:?}"),
            ));
        }
        self.capabilities.authorize(effect).map_err(|error| {
            guest_error(
                ErrorKind::PermissionDenied,
                bounded_text(error.message, self.hostcall_bytes),
            )
        })
    }
}

fn effect_covers(declared: &Effect, actual: &Effect) -> bool {
    match (declared, actual) {
        (Effect::FsRead { paths: allowed }, Effect::FsRead { paths: requested })
        | (Effect::FsWrite { paths: allowed }, Effect::FsWrite { paths: requested })
        | (
            Effect::FsDelete { paths: allowed, .. },
            Effect::FsDelete {
                paths: requested, ..
            },
        ) => requested.iter().all(|path| allowed.contains(path)),
        (Effect::EnvRead { names: allowed }, Effect::EnvRead { names: requested })
        | (Effect::EnvWrite { names: allowed }, Effect::EnvWrite { names: requested })
        | (Effect::SecretUse { names: allowed }, Effect::SecretUse { names: requested }) => {
            requested.iter().all(|name| allowed.contains(name))
        }
        _ => declared == actual,
    }
}

fn guest_error(kind: ErrorKind, message: String) -> GuestError {
    GuestError {
        kind,
        message,
        details_json: None,
    }
}
