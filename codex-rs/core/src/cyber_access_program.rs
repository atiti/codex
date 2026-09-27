//! Forward explicit programs while leaving entitlement and model policy to the server.

use codex_api::AccessPrograms;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::turn_input::CyberAccessProgram;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ApiKeyCyberAccessPrograms {
    UnsupportedProvider,
    Disabled,
    Enabled,
}

impl ApiKeyCyberAccessPrograms {
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        if config.model_provider_id != OPENAI_PROVIDER_ID {
            Self::UnsupportedProvider
        } else if config.features.enabled(Feature::ApiKeyCyberAccessPrograms) {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

pub(crate) fn for_provider(
    provider_id: &str,
    program: Option<CyberAccessProgram>,
) -> Option<CyberAccessProgram> {
    program.filter(|_| provider_id == OPENAI_PROVIDER_ID)
}

pub(crate) fn for_auth(
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
    program: Option<CyberAccessProgram>,
    policy: ApiKeyCyberAccessPrograms,
) -> Result<Option<AccessPrograms>> {
    if !provider.is_openai() {
        return Ok(None);
    }
    let Some(program) = program else {
        return Ok(None);
    };
    let Some(auth) = auth else {
        return Ok(None);
    };
    if auth.is_chatgpt_auth() {
        return Ok(Some(program.into()));
    }
    if !auth.is_api_key_auth() {
        return Ok(None);
    }
    match policy {
        ApiKeyCyberAccessPrograms::UnsupportedProvider => Ok(None),
        ApiKeyCyberAccessPrograms::Disabled => Err(CodexErr::InvalidRequest(
            "Cyber access programs are disabled for this API-key session.".to_owned(),
        )),
        ApiKeyCyberAccessPrograms::Enabled => Ok(Some(program.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_programs_require_both_chatgpt_auth_and_openai_destination() {
        let auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
        let openai = ModelProviderInfo::create_openai_provider(None);
        let azure = ModelProviderInfo {
            name: "agentroute-azure-direct".to_string(),
            base_url: Some("https://resource.cognitiveservices.azure.com/openai/v1".to_string()),
            ..Default::default()
        };
        let program = Some(CyberAccessProgram::DaybreakBlue);

        assert!(for_auth(Some(&auth), &openai, program, ApiKeyCyberAccessPrograms::Enabled).unwrap().is_some());
        assert!(for_auth(Some(&auth), &azure, program, ApiKeyCyberAccessPrograms::Enabled).unwrap().is_none());
        assert!(for_auth(None, &openai, program, ApiKeyCyberAccessPrograms::Enabled).unwrap().is_none());
    }
}
