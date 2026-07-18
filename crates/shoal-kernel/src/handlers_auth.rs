//! Live, explicitly-authorized administration of the durable bearer store.

use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenCreateParams {
    principal: String,
    #[serde(default = "default_profile")]
    profile: String,
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRevokeParams {
    id: String,
}

fn default_profile() -> String {
    "default".into()
}

impl Kernel {
    pub(crate) fn handle_auth_token_list(
        &self,
        attached: &Option<Attachment>,
    ) -> Result<Json, RpcError> {
        self.require_token_admin(attached)?;
        let store = self.lock_token_store()?;
        encode(store.try_list().map_err(token_store_error)?)
    }

    pub(crate) fn handle_auth_token_create(
        &self,
        params: Json,
        attached: &Option<Attachment>,
    ) -> Result<Json, RpcError> {
        self.require_token_admin(attached)?;
        let params: TokenCreateParams = decode(params)?;
        let ttl_ns = match params.ttl_seconds {
            Some(0) => {
                return Err(RpcError {
                    code: INVALID_PARAMS,
                    message: "ttl_seconds must be positive".into(),
                    data: None,
                });
            }
            Some(seconds) => Some(
                i64::try_from(seconds)
                    .ok()
                    .and_then(|seconds| seconds.checked_mul(1_000_000_000))
                    .ok_or_else(|| RpcError {
                        code: INVALID_PARAMS,
                        message: "ttl_seconds is too large".into(),
                        data: None,
                    })?,
            ),
            None => None,
        };
        let mut store = self.lock_token_store()?;
        let (bearer, meta) = store
            .create(params.principal, params.profile, params.caps, ttl_ns)
            .map_err(token_store_error)?;
        encode(json!({
            "token": bearer,
            "meta": meta,
            "secret_shown_once": true,
        }))
    }

    pub(crate) fn handle_auth_token_revoke(
        &self,
        params: Json,
        attached: &Option<Attachment>,
    ) -> Result<Json, RpcError> {
        self.require_token_admin(attached)?;
        let params: TokenRevokeParams = decode(params)?;
        let mut store = self.lock_token_store()?;
        let revoked = store.revoke(&params.id).map_err(token_store_error)?;
        encode(json!({"id": params.id, "revoked": revoked}))
    }

    fn require_token_admin<'a>(
        &self,
        attached: &'a Option<Attachment>,
    ) -> Result<&'a Attachment, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        if !attachment.can_admin_tokens() {
            return Err(RpcError {
                code: LEASH_DENIED,
                message: "token administration requires an embedded human trust root or an explicit token.admin machine credential".into(),
                data: Some(json!({"required_capability": "token.admin"})),
            });
        }
        Ok(attachment)
    }

    fn lock_token_store(&self) -> Result<std::sync::MutexGuard<'_, TokenStore>, RpcError> {
        self.auth
            .as_ref()
            .ok_or_else(|| RpcError {
                code: AUTH_FAILED,
                message: "token administration is unavailable in an ephemeral kernel".into(),
                data: Some(json!({"durable_kernel_required": true})),
            })?
            .lock()
            .map_err(|_| poisoned_subsystem("authentication token store"))
    }
}

fn token_store_error(error: io::Error) -> RpcError {
    if error.kind() == io::ErrorKind::InvalidInput {
        RpcError {
            code: INVALID_PARAMS,
            message: error.to_string(),
            data: None,
        }
    } else {
        RpcError {
            code: INTERNAL_ERROR,
            message: "token store operation failed".into(),
            data: Some(json!({"kind": format!("{:?}", error.kind())})),
        }
    }
}
