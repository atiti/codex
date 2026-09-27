use codex_api::AccessPrograms;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::turn_input::CyberAccessProgram;

pub(crate) fn for_auth(
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
    program: Option<CyberAccessProgram>,
) -> Option<AccessPrograms> {
    program
        .filter(|_| provider.is_openai() && auth.is_some_and(CodexAuth::is_chatgpt_auth))
        .map(AccessPrograms::from)
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

        assert!(for_auth(Some(&auth), &openai, program).is_some());
        assert!(for_auth(Some(&auth), &azure, program).is_none());
        assert!(for_auth(None, &openai, program).is_none());
    }
}
