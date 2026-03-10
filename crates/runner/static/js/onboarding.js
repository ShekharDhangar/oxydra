(function () {
  const STEP_LABELS = [
    'Welcome',
    'Runner Configuration',
    'Register First User',
    'Provider Setup',
    'Tools & Channels',
    'Review',
  ];

  // Sensible defaults per provider type for the wizard.
  const PROVIDER_DEFAULTS = {
    openai: {
      name: 'openai',
      envVar: 'OPENAI_API_KEY',
      model: 'gpt-4o-mini',
    },
    anthropic: {
      name: 'anthropic',
      envVar: 'ANTHROPIC_API_KEY',
      model: 'claude-sonnet-4-6',
    },
    gemini: {
      name: 'google-aistudio',
      envVar: 'GEMINI_API_KEY',
      model: 'gemini-2.0-flash',
    },
    openai_responses: {
      name: 'openai',
      envVar: 'OPENAI_API_KEY',
      model: 'gpt-4o-mini',
    },
  };

  function getProviderDefaults(type) {
    return PROVIDER_DEFAULTS[type] || null;
  }

  function inferDefaultUserConfigPath(userId) {
    const trimmed = (userId || '').trim();
    if (!trimmed) {
      return 'users/alice.toml';
    }
    return `users/${trimmed}.toml`;
  }

  function createOnboardingState() {
    return {
      step: 1,
      runnerWorkspaceRoot: 'workspaces',
      userId: 'alice',
      userConfigPath: 'users/alice.toml',
      existingUserId: '',
      existingUserConfigPath: '',
      canRenameDefaultUser: false,
      userRenamedFrom: '',
      providerName: 'openai',
      providerType: 'openai',
      providerApiKey: '',
      providerApiKeyEnv: 'OPENAI_API_KEY',
      defaultModel: 'gpt-4o-mini',
      providerEnvResolved: false,
      catalogModels: [],
      catalogRefreshing: false,
      customModelMode: false,
      shellEnabled: true,
      browserEnabled: true,
      browserCdpUrl: '',
      telegramEnabled: false,
      telegramBotTokenEnv: 'TELEGRAM_BOT_TOKEN',
      telegramSenderId: '',
      userAlreadyExists: false,
      busy: false,
      error: '',
      done: false,
    };
  }

  function stepLabel(step) {
    return STEP_LABELS[step - 1] || 'Setup';
  }

  window.OxydraOnboarding = {
    STEP_LABELS,
    PROVIDER_DEFAULTS,
    getProviderDefaults,
    inferDefaultUserConfigPath,
    createOnboardingState,
    stepLabel,
  };
})();
