const SECRET_MASK = '********';
const SECRET_SENTINEL = '__UNCHANGED__';

const ROUTES = {
  '/': 'dashboard',
  '/config/agent': 'config-agent',
  '/config/runner': 'config-runner',
  '/config/users': 'config-users',
  '/control': 'control',
  '/logs': 'logs',
  '/setup': 'setup',
};

async function api(path, options = {}) {
  const method = (options.method || 'GET').toUpperCase();
  const headers = {
    Accept: 'application/json',
    ...(options.headers || {}),
  };
  const request = {
    method,
    headers,
  };

  if (options.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    request.body = JSON.stringify(options.body);
  }

  const response = await fetch(`/api/v1${path}`, request);
  const payload = await response.json().catch(() => ({}));
  if (!response.ok || payload.error) {
    const message = payload.error?.message || `API request failed with status ${response.status}`;
    const error = new Error(message);
    error.code = payload.error?.code;
    error.status = response.status;
    throw error;
  }

  return payload.data;
}

function deepClone(value) {
  return JSON.parse(JSON.stringify(value));
}

function deepEqual(left, right) {
  return JSON.stringify(left) === JSON.stringify(right);
}

function isObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function inferFieldType(value) {
  if (value === SECRET_MASK) {
    return 'secret';
  }
  if (typeof value === 'boolean') {
    return 'boolean';
  }
  if (typeof value === 'number') {
    return 'number';
  }
  if (typeof value === 'string') {
    return 'string';
  }
  return 'json';
}

function createField(path, rawValue) {
  const type = inferFieldType(rawValue);
  if (type === 'secret') {
    return {
      path,
      type,
      value: '',
      original: SECRET_MASK,
      changed: false,
      touched: false,
      parseError: '',
    };
  }

  if (type === 'json') {
    return {
      path,
      type,
      value: JSON.stringify(rawValue, null, 2),
      original: deepClone(rawValue),
      changed: false,
      touched: false,
      parseError: '',
    };
  }

  return {
    path,
    type,
    value: rawValue,
    original: deepClone(rawValue),
    changed: false,
    touched: false,
    parseError: '',
  };
}

function buildFields(config, currentPath = '', fields = []) {
  if (Array.isArray(config)) {
    fields.push(createField(currentPath, config));
    return fields;
  }

  if (!isObject(config)) {
    fields.push(createField(currentPath, config));
    return fields;
  }

  const keys = Object.keys(config).sort();
  if (keys.length === 0 && currentPath) {
    fields.push(createField(currentPath, config));
    return fields;
  }

  keys.forEach((key) => {
    const nextPath = currentPath ? `${currentPath}.${key}` : key;
    const value = config[key];
    if (isObject(value)) {
      buildFields(value, nextPath, fields);
      return;
    }
    if (Array.isArray(value)) {
      fields.push(createField(nextPath, value));
      return;
    }
    fields.push(createField(nextPath, value));
  });

  return fields;
}

function setPatchPath(target, path, value) {
  const keys = path.split('.');
  let current = target;
  for (let idx = 0; idx < keys.length - 1; idx += 1) {
    const segment = keys[idx];
    if (!isObject(current[segment])) {
      current[segment] = {};
    }
    current = current[segment];
  }
  current[keys[keys.length - 1]] = value;
}

function parseFieldValue(field) {
  if (field.type === 'secret') {
    if (!field.touched) {
      return SECRET_SENTINEL;
    }
    return field.value;
  }
  if (field.type === 'boolean') {
    return Boolean(field.value);
  }
  if (field.type === 'number') {
    if (field.value === '' || field.value === null) {
      throw new Error(`Field ${field.path} must be a number`);
    }
    const parsed = Number(field.value);
    if (!Number.isFinite(parsed)) {
      throw new Error(`Field ${field.path} must be a finite number`);
    }
    return parsed;
  }
  if (field.type === 'json') {
    return JSON.parse(field.value);
  }
  return field.value;
}

function fieldHasChanges(field) {
  if (field.type === 'secret') {
    return field.touched;
  }
  try {
    const currentValue = parseFieldValue(field);
    return !deepEqual(currentValue, field.original);
  } catch {
    return true;
  }
}

function createEditor(response, endpoint) {
  return {
    endpoint,
    fileExists: response.file_exists,
    filePath: response.file_path,
    fields: buildFields(response.config),
    changedFields: [],
    backupPath: '',
    restartRequired: false,
  };
}

function onboardingFactory() {
  if (window.OxydraOnboarding?.createOnboardingState) {
    return window.OxydraOnboarding.createOnboardingState();
  }
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

function app() {
  return {
    currentPage: 'dashboard',
    connected: false,
    loading: false,
    saving: false,
    sidebarOpen: false,
    meta: {},
    onboarding: { needs_setup: false, checks: {} },
    statusUsers: [],
    userList: [],
    runnerEditor: null,
    runnerStructuredEditor: null,
    runnerConfigRaw: null,
    agentEditor: null,
    agentStructuredEditor: null,
    agentConfigRaw: null,
    userEditor: null,
    userStructuredEditor: null,
    userConfigRaw: null,
    selectedUserId: '',
    restartNotice: '',
    schemaCache: null,
    catalogCache: null,
    catalogStatusCache: null,
    toast: {
      visible: false,
      message: '',
      kind: 'info',
    },
    newUser: {
      user_id: '',
      config_path: '',
    },
    pageError: '',
    onboardingWizard: onboardingFactory(),
    controlState: window.OxydraControl?.createControlState
      ? window.OxydraControl.createControlState()
      : { busyByUser: {} },
    logsState: window.OxydraLogs?.createLogsState
      ? window.OxydraLogs.createLogsState()
      : {
        userId: '',
        role: 'runtime',
        stream: 'both',
        tail: 200,
        format: 'json',
        autoRefresh: true,
        entries: [],
        warnings: [],
        truncated: false,
        loading: false,
      },

    async init() {
      window.addEventListener('hashchange', () => this.route());
      this._controlPollTimer = window.setInterval(() => {
        if (document.visibilityState !== 'visible' || this.currentPage !== 'control') {
          return;
        }
        this.loadStatus().catch((error) => this.showToast(error.message, 'error'));
      }, 5000);
      this._logsPollTimer = window.setInterval(() => {
        if (
          document.visibilityState !== 'visible'
          || this.currentPage !== 'logs'
          || !this.logsState.autoRefresh
          || !this.logsState.userId
        ) {
          return;
        }
        this.refreshLogs().catch((error) => this.showToast(error.message, 'error'));
      }, 2000);
      await this.refreshCoreData();
      this.route();
    },

    async refreshCoreData() {
      try {
        this.meta = await api('/meta');
        this.connected = true;
      } catch (error) {
        this.connected = false;
        this.showToast(error.message, 'error');
      }

      await this.refreshOnboardingStatus();
      await this.loadStatus();
    },

    async loadSchema() {
      if (this.schemaCache) return this.schemaCache;
      this.schemaCache = await api('/meta/schema');
      return this.schemaCache;
    },

    async loadCatalog() {
      if (this.catalogCache) return this.catalogCache;
      this.catalogCache = await api('/catalog');
      return this.catalogCache;
    },

    async loadCatalogStatus() {
      this.catalogStatusCache = await api('/catalog/status');
      return this.catalogStatusCache;
    },

    async refreshCatalog() {
      const result = await api('/catalog/refresh', { method: 'POST', body: {} });
      // Invalidate catalog cache so it gets reloaded
      this.catalogCache = null;
      return result;
    },

    route() {
      const hash = window.location.hash.slice(1) || '/';
      this.currentPage = ROUTES[hash] || 'dashboard';
      this.sidebarOpen = false;
      this.onPageChange();
    },

    async onPageChange() {
      this.loading = true;
      this.pageError = '';
      try {
        if (this.currentPage === 'dashboard') {
          await this.loadStatus();
        } else if (this.currentPage === 'config-agent') {
          // Always flush schema cache so newly-added providers appear in selection
          this.schemaCache = null;
          await this.loadAgentConfig();
        } else if (this.currentPage === 'config-runner') {
          await this.loadRunnerConfig();
        } else if (this.currentPage === 'config-users') {
          await this.loadUsersPage();
        } else if (this.currentPage === 'control') {
          await this.loadControlPage();
        } else if (this.currentPage === 'logs') {
          await this.loadLogsPage();
        } else if (this.currentPage === 'setup') {
          await this.seedOnboardingWizard();
        }
      } catch (error) {
        this.pageError = error.message || 'An unexpected error occurred.';
        this.showToast(error.message, 'error');
      } finally {
        this.loading = false;
      }
    },

    async refreshOnboardingStatus() {
      try {
        this.onboarding = await api('/onboarding/status');
      } catch (error) {
        this.showToast(error.message, 'error');
      }
    },

    async loadStatus() {
      const data = await api('/status');
      this.statusUsers = data.users || [];
    },

    async loadRunnerConfig() {
      // Load schema and config in parallel
      const [schemaData, response] = await Promise.all([
        this.loadSchema(),
        api('/config/runner'),
      ]);

      // Store raw response for save flow
      this.runnerConfigRaw = response;

      // Keep the old editor for backward compat of save flow
      this.runnerEditor = createEditor(response, '/config/runner');
      const workspaceField = this.runnerEditor.fields.find((field) => field.path === 'workspace_root');
      if (workspaceField) {
        this.onboardingWizard.runnerWorkspaceRoot = workspaceField.value;
      }

      // Render the structured editor
      const container = document.getElementById('runner-structured-editor');
      if (container && schemaData && window.RunnerConfigEditor) {
        const self = this;
        this.runnerStructuredEditor = window.RunnerConfigEditor.render(container, {
          schema: schemaData.runner,
          config: response.config,
          dynamicSources: schemaData.dynamic_sources,
          fileExists: response.file_exists,
          filePath: response.file_path,
          showToast: (msg, kind) => self.showToast(msg, kind),
        });
      }
    },

    async loadAgentConfig() {
      // Load schema, catalog, and config in parallel
      const [schemaData, catalogData, catalogStatus, response] = await Promise.all([
        this.loadSchema(),
        this.loadCatalog(),
        this.loadCatalogStatus(),
        api('/config/agent'),
      ]);

      // Store raw response for save flow
      this.agentConfigRaw = response;

      // Keep the old editor for backward compat of save flow
      this.agentEditor = createEditor(response, '/config/agent');

      // Render the structured editor
      const container = document.getElementById('agent-structured-editor');
      if (container && schemaData && window.AgentConfigEditor) {
        const self = this;
        this.agentStructuredEditor = window.AgentConfigEditor.render(container, {
          schema: schemaData.agent,
          config: response.config,
          dynamicSources: schemaData.dynamic_sources,
          catalog: catalogData.providers || [],
          catalogStatus: catalogStatus,
          fileExists: response.file_exists,
          filePath: response.file_path,
          showToast: (msg, kind) => self.showToast(msg, kind),
          onRefreshCatalog: async () => {
            try {
              const result = await self.refreshCatalog();
              self.showToast('Catalog refreshed successfully.', 'success');
              return result;
            } catch (error) {
              self.showToast(error.message, 'error');
              throw error;
            }
          },
        });
      }
    },

    async loadUsersPage() {
      await this.loadUsers();
      await this.loadStatus();
      if (this.selectedUserId) {
        const exists = this.userList.some((user) => user.user_id === this.selectedUserId);
        if (exists) {
          await this.loadUserConfig(this.selectedUserId);
          return;
        }
      }

      // Don't auto-load first user — wait for explicit "Edit" click
      this.selectedUserId = '';
      this.userEditor = null;
    },

    closeUserEditor() {
      this.selectedUserId = '';
      this.userEditor = null;
      this.userStructuredEditor = null;
    },

    async loadUsers() {
      const data = await api('/config/users');
      this.userList = data.users || [];
    },

    async loadUserConfig(userId) {
      this.selectedUserId = userId;
      const encodedUser = encodeURIComponent(userId);

      // Load schema and config in parallel
      const [schemaData, response] = await Promise.all([
        this.loadSchema(),
        api(`/config/users/${encodedUser}`),
      ]);

      // Store raw response for save flow
      this.userConfigRaw = response;

      // Keep the old editor for backward compat of save flow
      this.userEditor = createEditor(response, `/config/users/${encodedUser}`);

      // Render the structured editor
      await this.$nextTick();
      const container = document.getElementById('user-structured-editor');
      if (container && schemaData && window.UserConfigEditor) {
        const self = this;
        this.userStructuredEditor = window.UserConfigEditor.render(container, {
          schema: schemaData.user,
          config: response.config,
          dynamicSources: schemaData.dynamic_sources,
          fileExists: response.file_exists,
          filePath: response.file_path,
          showToast: (msg, kind) => self.showToast(msg, kind),
        });
      }
    },

    findUserStatus(userId) {
      return this.statusUsers.find((entry) => entry.user_id === userId);
    },

    isUserRunning(userId) {
      const status = this.findUserStatus(userId);
      return Boolean(status && status.daemon_running);
    },

    controlUserBusy(userId) {
      if (window.OxydraControl?.isUserBusy) {
        return window.OxydraControl.isUserBusy(this.controlState, userId);
      }
      return Boolean(this.controlState.busyByUser[userId]);
    },

    setControlUserBusy(userId, busy) {
      if (window.OxydraControl?.setUserBusy) {
        window.OxydraControl.setUserBusy(this.controlState, userId, busy);
        return;
      }
      this.controlState.busyByUser[userId] = Boolean(busy);
    },

    async loadControlPage() {
      await this.loadUsers();
      await this.loadStatus();
    },

    async runControlAction(userId, action) {
      if (!userId || this.controlUserBusy(userId)) {
        return;
      }
      this.setControlUserBusy(userId, true);
      try {
        await api(`/control/${encodeURIComponent(userId)}/${action}`, {
          method: 'POST',
          body: {},
        });
        await this.loadStatus();
        this.showToast(`User ${userId}: ${action} completed.`, 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.setControlUserBusy(userId, false);
      }
    },

    async loadLogsPage() {
      await this.loadUsers();
      await this.loadStatus();
      const hasSelectedUser = this.userList.some((user) => user.user_id === this.logsState.userId);
      if (!hasSelectedUser) {
        this.logsState.userId = this.userList[0]?.user_id || '';
      }
      if (!this.logsState.userId) {
        this.logsState.entries = [];
        this.logsState.warnings = [];
        this.logsState.truncated = false;
        return;
      }
      await this.refreshLogs();
    },

    async setLogsUser(userId) {
      this.logsState.userId = userId;
      await this.refreshLogs();
    },

    logEntryLevel(entry) {
      if (window.OxydraLogs?.inferLevel) {
        return window.OxydraLogs.inferLevel(entry);
      }
      const message = String(entry?.message || '').toLowerCase();
      if (message.includes('error')) return 'error';
      if (message.includes('warn')) return 'warn';
      if (message.includes('debug')) return 'debug';
      if (message.includes('trace')) return 'trace';
      return 'info';
    },

    logEntryText(entry) {
      if (window.OxydraLogs?.toTextLine) {
        return window.OxydraLogs.toTextLine(entry);
      }
      const ts = entry?.timestamp || '-';
      return `${ts} [${entry.source || 'process_file'}][${entry.role}][${entry.stream}] ${entry.message || ''}`;
    },

    decorateLogEntries(entries) {
      return (entries || []).map((entry) => ({
        ...entry,
        _level: this.logEntryLevel(entry),
        _text: this.logEntryText(entry),
      }));
    },

    async refreshLogs() {
      if (!this.logsState.userId) {
        return;
      }
      this.logsState.loading = true;
      try {
        const params = new URLSearchParams({
          role: this.logsState.role,
          stream: this.logsState.stream,
          tail: String(this.logsState.tail || 200),
          format: this.logsState.format || 'json',
        });
        const data = await api(`/logs/${encodeURIComponent(this.logsState.userId)}?${params.toString()}`);
        this.logsState.entries = this.decorateLogEntries(data.entries);
        this.logsState.warnings = data.warnings || [];
        this.logsState.truncated = Boolean(data.truncated);
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.logsState.loading = false;
      }
    },

    async copyLogsToClipboard() {
      const text = this.logsState.entries
        .map((entry) => entry._text || this.logEntryText(entry))
        .join('\n');
      if (!text) {
        this.showToast('No log lines to copy.', 'info');
        return;
      }
      try {
        await navigator.clipboard.writeText(text);
        this.showToast('Copied logs to clipboard.', 'success');
      } catch {
        this.showToast('Clipboard copy failed.', 'error');
      }
    },

    updateTextField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    updateBooleanField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    updateNumberField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    updateJsonField(editor, field, nextValue) {
      field.value = nextValue;
      field.touched = true;
      field.changed = fieldHasChanges(field);
      field.parseError = '';
      if (editor) {
        editor.changedFields = [];
      }
    },

    clearSecretField(editor, field) {
      field.value = '';
      field.touched = true;
      field.changed = true;
      if (editor) {
        editor.changedFields = [];
      }
    },

    editorHasChanges(editor) {
      return Boolean(editor && editor.fields.some((field) => fieldHasChanges(field)));
    },

    agentHasChanges() {
      if (this.agentStructuredEditor) {
        return this.agentStructuredEditor.hasChanges();
      }
      return this.editorHasChanges(this.agentEditor);
    },

    buildEditorPatch(editor) {
      const patch = {};
      let hasUserChanges = false;

      editor.fields.forEach((field) => {
        if (field.type === 'secret') {
          const value = field.touched ? field.value : SECRET_SENTINEL;
          setPatchPath(patch, field.path, value);
          if (field.touched) {
            hasUserChanges = true;
          }
          return;
        }

        if (!field.changed) {
          return;
        }

        try {
          const parsed = parseFieldValue(field);
          setPatchPath(patch, field.path, parsed);
          field.parseError = '';
          hasUserChanges = true;
        } catch (error) {
          field.parseError = error.message;
          throw error;
        }
      });

      return { patch, hasUserChanges };
    },

    async saveEditor(editor, label, reloadCallback) {
      if (!editor || this.saving) {
        return;
      }

      let patchPayload;
      try {
        patchPayload = this.buildEditorPatch(editor);
      } catch (error) {
        this.showToast(error.message, 'error');
        return;
      }

      if (!patchPayload.hasUserChanges) {
        this.showToast(`No changes to save for ${label}.`, 'info');
        return;
      }

      this.saving = true;
      try {
        const validation = await api(`${editor.endpoint}/validate`, {
          method: 'POST',
          body: patchPayload.patch,
        });
        const changedFields = validation.changed_fields || [];
        editor.changedFields = changedFields;

        if (changedFields.length === 0) {
          this.showToast(`No effective changes for ${label}.`, 'info');
          return;
        }

        const preview = changedFields.slice(0, 12).join('\n');
        const accepted = window.confirm(
          `Save ${label}?\n\nChanged fields:\n${preview}${changedFields.length > 12 ? '\n...' : ''}`
        );
        if (!accepted) {
          return;
        }

        const result = await api(editor.endpoint, {
          method: 'PATCH',
          body: patchPayload.patch,
        });

        editor.changedFields = result.changed_fields || [];
        editor.backupPath = result.backup_path || '';
        editor.restartRequired = Boolean(result.restart_required);
        if (editor.restartRequired) {
          this.restartNotice = 'Configuration was saved. Restart is required for running daemon(s).';
        }

        await reloadCallback();
        await this.loadStatus();
        await this.refreshOnboardingStatus();

        this.showToast(`${label} saved successfully.`, 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async saveRunnerConfig() {
      if (this.saving) return;

      // Use structured editor if available
      if (this.runnerStructuredEditor) {
        await this.saveStructuredRunnerConfig();
        return;
      }

      // Fallback to legacy editor
      await this.saveEditor(this.runnerEditor, 'Runner configuration', async () => {
        await this.loadRunnerConfig();
      });
    },

    async saveStructuredRunnerConfig() {
      if (!this.runnerStructuredEditor || this.saving) return;

      const patchResult = this.runnerStructuredEditor.getPatch();
      if (!patchResult.hasChanges) {
        this.showToast('No changes to save for Runner configuration.', 'info');
        return;
      }

      this.saving = true;
      try {
        const endpoint = '/config/runner';
        const patch = patchResult.patch;

        // Validate first
        const validation = await api(`${endpoint}/validate`, {
          method: 'POST',
          body: patch,
        });
        const changedFields = validation.changed_fields || [];

        if (changedFields.length === 0) {
          this.showToast('No effective changes for Runner configuration.', 'info');
          return;
        }

        const result = await api(endpoint, {
          method: 'PATCH',
          body: patch,
        });

        if (result.restart_required) {
          this.restartNotice = 'Configuration was saved. Restart is required for running daemon(s).';
        }

        // Reload the page
        await this.loadRunnerConfig();
        await this.loadStatus();
        await this.refreshOnboardingStatus();

        this.showToast('Runner configuration saved successfully.', 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async saveAgentConfig() {
      if (this.saving) return;

      // Use structured editor if available
      if (this.agentStructuredEditor) {
        await this.saveStructuredAgentConfig();
        return;
      }

      // Fallback to legacy editor
      await this.saveEditor(this.agentEditor, 'Agent configuration', async () => {
        await this.loadAgentConfig();
      });
    },

    async saveStructuredAgentConfig() {
      if (!this.agentStructuredEditor || this.saving) return;

      const patchResult = this.agentStructuredEditor.getPatch();
      if (!patchResult.hasChanges) {
        this.showToast('No changes to save for Agent configuration.', 'info');
        return;
      }

      this.saving = true;
      try {
        const endpoint = '/config/agent';
        const patch = patchResult.patch;

        // Validate first
        const validation = await api(`${endpoint}/validate`, {
          method: 'POST',
          body: patch,
        });
        const changedFields = validation.changed_fields || [];

        if (changedFields.length === 0) {
          this.showToast('No effective changes for Agent configuration.', 'info');
          return;
        }

        const result = await api(endpoint, {
          method: 'PATCH',
          body: patch,
        });

        if (result.restart_required) {
          this.restartNotice = 'Configuration was saved. Restart is required for running daemon(s).';
        }

        // Invalidate schema cache so newly added providers appear in the selection dropdown
        this.schemaCache = null;
        // Reload the page
        await this.loadAgentConfig();
        await this.loadStatus();
        await this.refreshOnboardingStatus();

        this.showToast('Agent configuration saved successfully.', 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async saveUserConfig() {
      if (!this.selectedUserId || this.saving) {
        return;
      }

      // Use structured editor if available
      if (this.userStructuredEditor) {
        await this.saveStructuredUserConfig();
        return;
      }

      // Fallback to legacy editor
      await this.saveEditor(this.userEditor, `User ${this.selectedUserId} configuration`, async () => {
        await this.loadUserConfig(this.selectedUserId);
      });
    },

    async saveStructuredUserConfig() {
      if (!this.userStructuredEditor || this.saving) return;

      const patchResult = this.userStructuredEditor.getPatch();
      if (!patchResult.hasChanges) {
        this.showToast(`No changes to save for User ${this.selectedUserId} configuration.`, 'info');
        return;
      }

      this.saving = true;
      try {
        const endpoint = `/config/users/${encodeURIComponent(this.selectedUserId)}`;
        const patch = patchResult.patch;

        // Validate first
        const validation = await api(`${endpoint}/validate`, {
          method: 'POST',
          body: patch,
        });
        const changedFields = validation.changed_fields || [];

        if (changedFields.length === 0) {
          this.showToast(`No effective changes for User ${this.selectedUserId} configuration.`, 'info');
          return;
        }

        const result = await api(endpoint, {
          method: 'PATCH',
          body: patch,
        });

        if (result.restart_required) {
          this.restartNotice = 'Configuration was saved. Restart is required for running daemon(s).';
        }

        // Reload the page
        await this.loadUserConfig(this.selectedUserId);
        await this.loadStatus();
        await this.refreshOnboardingStatus();

        this.showToast(`User ${this.selectedUserId} configuration saved successfully.`, 'success');
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async addUser() {
      if (!this.newUser.user_id.trim()) {
        this.showToast('User ID is required.', 'error');
        return;
      }
      if (!this.newUser.config_path.trim()) {
        this.newUser.config_path = this.defaultUserConfigPath(this.newUser.user_id);
      }

      this.saving = true;
      try {
        await api('/config/users', {
          method: 'POST',
          body: {
            user_id: this.newUser.user_id.trim(),
            config_path: this.newUser.config_path.trim(),
          },
        });

        this.showToast(`User ${this.newUser.user_id.trim()} was added.`, 'success');
        this.newUser.user_id = '';
        this.newUser.config_path = '';
        await this.loadUsersPage();
        await this.refreshOnboardingStatus();
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    async deleteUser(userId) {
      const confirmed = window.confirm(`Delete user ${userId}? This also deletes the user config file.`);
      if (!confirmed) {
        return;
      }

      this.saving = true;
      try {
        await api(`/config/users/${encodeURIComponent(userId)}?delete_config_file=true`, {
          method: 'DELETE',
          body: {},
        });
        this.showToast(`User ${userId} was removed.`, 'success');
        await this.loadUsersPage();
        await this.refreshOnboardingStatus();
      } catch (error) {
        this.showToast(error.message, 'error');
      } finally {
        this.saving = false;
      }
    },

    defaultUserConfigPath(userId) {
      if (window.OxydraOnboarding?.inferDefaultUserConfigPath) {
        return window.OxydraOnboarding.inferDefaultUserConfigPath(userId);
      }
      const trimmed = (userId || '').trim();
      return trimmed ? `users/${trimmed}.toml` : 'users/alice.toml';
    },

    async seedOnboardingWizard() {
      try {
        this.onboardingWizard = onboardingFactory();
        const [runnerResponse, agentResponse, usersResponse] = await Promise.all([
          api('/config/runner'),
          api('/config/agent'),
          api('/config/users'),
        ]);
        const runnerConfig = runnerResponse.config || {};
        const agentConfig = agentResponse.config || {};
        const users = usersResponse.users || [];
        const selection = agentConfig.selection || {};
        const providerRegistry = (agentConfig.providers && agentConfig.providers.registry) || {};
        const activeProviderName = selection.provider || this.onboardingWizard.providerName;
        const activeProvider = providerRegistry[activeProviderName] || null;
        const shellConfig = (agentConfig.tools && agentConfig.tools.shell) || {};
        const browserConfig = (agentConfig.tools && agentConfig.tools.browser) || {};

        if (runnerConfig.workspace_root) {
          this.onboardingWizard.runnerWorkspaceRoot = runnerConfig.workspace_root;
        }
        if (selection.provider) {
          this.onboardingWizard.providerName = selection.provider;
        }
        if (selection.model) {
          this.onboardingWizard.defaultModel = selection.model;
        }
        if (activeProvider && activeProvider.provider_type) {
          this.onboardingWizard.providerType = activeProvider.provider_type;
        }
        if (activeProvider && activeProvider.api_key_env) {
          this.onboardingWizard.providerApiKeyEnv = activeProvider.api_key_env;
        }
        this.onboardingWizard.shellEnabled = shellConfig.enabled == null
          ? true
          : Boolean(shellConfig.enabled);
        this.onboardingWizard.browserEnabled = browserConfig.enabled == null
          ? true
          : Boolean(browserConfig.enabled);
        this.onboardingWizard.browserCdpUrl = browserConfig.cdp_url || '';

        if (users.length > 0) {
          const firstUser = users[0];
          this.userList = users;
          this.onboardingWizard.userId = firstUser.user_id;
          this.onboardingWizard.userConfigPath = firstUser.config_path;
          this.onboardingWizard.existingUserId = firstUser.user_id;
          this.onboardingWizard.existingUserConfigPath = firstUser.config_path;
          this.onboardingWizard.userAlreadyExists = true;
          this.onboardingWizard.canRenameDefaultUser = users.length === 1
            && firstUser.user_id === 'alice'
            && firstUser.config_path === 'users/alice.toml';

          const userResponse = await api(`/config/users/${encodeURIComponent(firstUser.user_id)}`);
          const userConfig = userResponse.config || {};
          const telegram = userConfig.channels && userConfig.channels.telegram;
          const firstSender = Array.isArray(telegram && telegram.senders) && telegram.senders.length > 0
            ? telegram.senders[0]
            : null;
          const firstSenderId = firstSender && Array.isArray(firstSender.platform_ids)
            ? (firstSender.platform_ids[0] || '')
            : '';

          this.onboardingWizard.telegramEnabled = Boolean(telegram && telegram.enabled);
          this.onboardingWizard.telegramBotTokenEnv = (telegram && telegram.bot_token_env)
            || this.onboardingWizard.telegramBotTokenEnv
            || 'TELEGRAM_BOT_TOKEN';
          this.onboardingWizard.telegramSenderId = firstSenderId;
        } else {
          this.onboardingWizard.userAlreadyExists = false;
          this.onboardingWizard.existingUserId = '';
          this.onboardingWizard.existingUserConfigPath = '';
          this.onboardingWizard.canRenameDefaultUser = false;
          this.onboardingWizard.userConfigPath = this.defaultUserConfigPath(this.onboardingWizard.userId);
        }

        await this.onboardingLoadCatalogModels(this.onboardingWizard.providerType);
      } catch (error) {
        this.showToast(error.message, 'error');
      }
    },

    onboardingStepLabel() {
      if (window.OxydraOnboarding?.stepLabel) {
        return window.OxydraOnboarding.stepLabel(this.onboardingWizard.step);
      }
      return `Step ${this.onboardingWizard.step}`;
    },

    onboardingProviderTypeChange(type) {
      this.onboardingWizard.providerType = type;
      this.onboardingWizard.customModelMode = false;
      const defaults = window.OxydraOnboarding?.getProviderDefaults
        ? window.OxydraOnboarding.getProviderDefaults(type)
        : null;
      if (defaults) {
        this.onboardingWizard.providerName = defaults.name;
        this.onboardingWizard.providerApiKeyEnv = defaults.envVar;
        this.onboardingWizard.defaultModel = defaults.model;
      }
      this.onboardingLoadCatalogModels(type);
    },

    async onboardingLoadCatalogModels(providerType) {
      // Try multiple catalog provider IDs in order of preference.
      // The pinned snapshot uses "google" for Gemini; live models.dev may use "gemini".
      const candidateIds = {
        openai: ['openai'],
        anthropic: ['anthropic'],
        gemini: ['google', 'gemini', 'google-aistudio'],
        openai_responses: ['openai'],
      }[providerType] || [providerType];

      try {
        const catalog = await this.loadCatalog();
        const providers = catalog.providers || [];
        let provider = null;
        for (const id of candidateIds) {
          provider = providers.find(p => p.id === id);
          if (provider) break;
        }
        this.onboardingWizard.catalogModels = provider
          ? provider.models.map(m => m.id).sort()
          : [];
      } catch (_e) {
        this.onboardingWizard.catalogModels = [];
      }
    },

    async onboardingRefreshCatalog() {
      this.onboardingWizard.catalogRefreshing = true;
      try {
        await api('/catalog/refresh', { method: 'POST', body: {} });
        this.catalogCache = null;
        await this.onboardingLoadCatalogModels(this.onboardingWizard.providerType);
        this.showToast('Catalog refreshed.', 'success');
      } catch (e) {
        this.showToast('Catalog refresh failed: ' + e.message, 'error');
      } finally {
        this.onboardingWizard.catalogRefreshing = false;
      }
    },

    onboardingModelSelectChange(val) {
      if (val === '__custom__') {
        this.onboardingWizard.customModelMode = true;
        this.onboardingWizard.defaultModel = '';
      } else {
        this.onboardingWizard.defaultModel = val;
      }
    },

    // Called by x-effect to imperatively populate the model <select>.
    // More reliable than x-for inside <select> with x-show in Alpine.js.
    onboardingBuildModelSelect(el) {
      const models = this.onboardingWizard.catalogModels;
      const current = this.onboardingWizard.defaultModel;
      el.innerHTML = '';
      models.forEach(m => {
        const opt = document.createElement('option');
        opt.value = m;
        opt.textContent = m;
        opt.selected = m === current;
        el.appendChild(opt);
      });
      const customOpt = document.createElement('option');
      customOpt.value = '__custom__';
      customOpt.textContent = '— Enter custom model ID —';
      el.appendChild(customOpt);
      // Ensure current value is selected; if not in list, pick first model
      if (current && el.value !== current && current !== '__custom__') {
        if (models.includes(current)) {
          el.value = current;
        } else if (models.length > 0) {
          el.value = models[0];
          this.onboardingWizard.defaultModel = models[0];
        }
      }
    },

    onboardingSummary() {
      return [
        `Workspace root: ${this.onboardingWizard.runnerWorkspaceRoot || '(not set)'}`,
        `User: ${this.onboardingWizard.userId || '(not set)'}`,
        `User config path: ${this.onboardingWizard.userConfigPath || '(not set)'}`,
        this.onboardingWizard.userRenamedFrom
          ? `Built-in user renamed: ${this.onboardingWizard.userRenamedFrom} -> ${this.onboardingWizard.userId}`
          : 'Built-in user renamed: no',
        `Provider: ${this.onboardingWizard.providerName || '(not set)'}`,
        `Provider type: ${this.onboardingWizard.providerType || '(not set)'}`,
        `Default model: ${this.onboardingWizard.defaultModel || '(not set)'}`,
        this.onboardingWizard.providerApiKey
          ? 'Provider credential: inline API key'
          : `Provider credential env: ${this.onboardingWizard.providerApiKeyEnv || '(not set)'}`,
        `Shell tool default: ${this.onboardingWizard.shellEnabled ? 'enabled' : 'disabled'}`,
        `Browser tool default: ${this.onboardingWizard.browserEnabled ? 'enabled' : 'disabled'}${this.onboardingWizard.browserCdpUrl ? ` (CDP: ${this.onboardingWizard.browserCdpUrl})` : ''}`,
        this.onboardingWizard.telegramEnabled
          ? `Telegram: enabled via ${this.onboardingWizard.telegramBotTokenEnv || '(env not set)'} for sender ${this.onboardingWizard.telegramSenderId || '(sender not set)'}`
          : 'Telegram: not configured',
      ];
    },

    onboardingAutofillUserPath() {
      this.onboardingWizard.userConfigPath = this.defaultUserConfigPath(this.onboardingWizard.userId);
    },

    onboardingUserIdInput(value) {
      this.onboardingWizard.userId = value;
      this.onboardingWizard.userConfigPath = this.defaultUserConfigPath(value);
    },

    onboardingPreviousStep() {
      if (this.onboardingWizard.step > 1) {
        this.onboardingWizard.step -= 1;
      }
    },

    async onboardingNextStep() {
      if (this.onboardingWizard.busy) {
        return;
      }

      this.onboardingWizard.error = '';

      try {
        this.onboardingWizard.busy = true;

        if (this.onboardingWizard.step === 1) {
          this.onboardingWizard.step = 2;
          return;
        }

        if (this.onboardingWizard.step === 2) {
          if (!this.onboardingWizard.runnerWorkspaceRoot.trim()) {
            throw new Error('Workspace root is required.');
          }

          const patch = {
            workspace_root: this.onboardingWizard.runnerWorkspaceRoot.trim(),
          };
          await api('/config/runner/validate', { method: 'POST', body: patch });
          await api('/config/runner', { method: 'PATCH', body: patch });
          await this.loadRunnerConfig();
          this.onboardingWizard.step = 3;
          this.showToast('Runner configuration saved.', 'success');
          return;
        }

        if (this.onboardingWizard.step === 3) {
          const requestedUserId = this.onboardingWizard.userId.trim();
          if (!requestedUserId) {
            throw new Error('User ID is required.');
          }

          const requestedConfigPath = this.defaultUserConfigPath(requestedUserId);
          this.onboardingWizard.userConfigPath = requestedConfigPath;

          if (this.onboardingWizard.canRenameDefaultUser
            && this.onboardingWizard.existingUserId
            && this.onboardingWizard.existingUserId !== requestedUserId) {
            await api('/config/users/rename', {
              method: 'POST',
              body: {
                old_user_id: this.onboardingWizard.existingUserId,
                new_user_id: requestedUserId,
                new_config_path: requestedConfigPath,
              },
            });
            this.onboardingWizard.userRenamedFrom = this.onboardingWizard.existingUserId;
            this.onboardingWizard.existingUserId = requestedUserId;
            this.onboardingWizard.existingUserConfigPath = requestedConfigPath;
            this.onboardingWizard.userAlreadyExists = true;
            this.onboardingWizard.canRenameDefaultUser = false;
          } else if (!this.onboardingWizard.existingUserId) {
            await api('/config/users', {
              method: 'POST',
              body: {
                user_id: requestedUserId,
                config_path: requestedConfigPath,
              },
            });
            this.onboardingWizard.existingUserId = requestedUserId;
            this.onboardingWizard.existingUserConfigPath = requestedConfigPath;
            this.onboardingWizard.userAlreadyExists = true;
          } else if (this.onboardingWizard.existingUserId !== requestedUserId) {
            await api('/config/users', {
              method: 'POST',
              body: {
                user_id: requestedUserId,
                config_path: requestedConfigPath,
              },
            });
            this.onboardingWizard.existingUserId = requestedUserId;
            this.onboardingWizard.existingUserConfigPath = requestedConfigPath;
            this.onboardingWizard.userAlreadyExists = true;
          }

          this.onboardingWizard.userId = requestedUserId;
          this.onboardingWizard.userConfigPath = requestedConfigPath;
          await this.loadUsersPage();
          this.onboardingWizard.step = 4;
          this.showToast('User registration completed.', 'success');
          // Pre-load catalog models for the default provider type
          await this.onboardingLoadCatalogModels(this.onboardingWizard.providerType);
          return;
        }

        if (this.onboardingWizard.step === 4) {
          const providerName = this.onboardingWizard.providerName.trim();
          if (!providerName) {
            throw new Error('Provider name is required.');
          }

          const providerPatch = {
            providers: {
              registry: {
                [providerName]: {
                  provider_type: this.onboardingWizard.providerType,
                },
              },
            },
            selection: {
              provider: providerName,
              model: this.onboardingWizard.defaultModel.trim() || 'gpt-4o-mini',
            },
          };

          if (this.onboardingWizard.providerApiKey.trim()) {
            providerPatch.providers.registry[providerName].api_key = this.onboardingWizard.providerApiKey.trim();
          } else if (this.onboardingWizard.providerApiKeyEnv.trim()) {
            providerPatch.providers.registry[providerName].api_key_env = this.onboardingWizard.providerApiKeyEnv.trim();
          } else {
            throw new Error('Provide an API key or an API key env var name.');
          }

          await api('/config/agent/validate', { method: 'POST', body: providerPatch });
          await api('/config/agent', { method: 'PATCH', body: providerPatch });
          // Invalidate schema cache so the new provider appears in the selection dropdown
          this.schemaCache = null;
          await this.loadAgentConfig();
          await this.refreshOnboardingStatus();
          this.onboardingWizard.providerEnvResolved = Boolean(this.onboarding.checks?.has_provider);
          this.onboardingWizard.step = 5;
          this.showToast('Provider setup saved.', 'success');
          return;
        }

        if (this.onboardingWizard.step === 5) {
          const toolPatch = {
            tools: {
              shell: {
                enabled: Boolean(this.onboardingWizard.shellEnabled),
              },
              browser: {
                enabled: Boolean(this.onboardingWizard.browserEnabled),
                cdp_url: this.onboardingWizard.browserCdpUrl.trim() || null,
              },
            },
          };
          const userEndpoint = `/config/users/${encodeURIComponent(this.onboardingWizard.userId.trim())}`;
          const userPatch = this.onboardingWizard.telegramEnabled
            ? {
              channels: {
                telegram: {
                  enabled: true,
                  bot_token_env: this.onboardingWizard.telegramBotTokenEnv.trim(),
                  senders: [{
                    platform_ids: [this.onboardingWizard.telegramSenderId.trim()],
                  }],
                },
              },
            }
            : {
              channels: {
                telegram: null,
              },
            };

          if (this.onboardingWizard.telegramEnabled) {
            if (!this.onboardingWizard.telegramBotTokenEnv.trim()) {
              throw new Error('Telegram bot token env var is required when Telegram is enabled.');
            }
            if (!this.onboardingWizard.telegramSenderId.trim()) {
              throw new Error('Telegram sender ID is required when Telegram is enabled.');
            }
          }

          await api('/config/agent/validate', { method: 'POST', body: toolPatch });
          await api(`${userEndpoint}/validate`, { method: 'POST', body: userPatch });
          await api('/config/agent', { method: 'PATCH', body: toolPatch });
          await api(userEndpoint, { method: 'PATCH', body: userPatch });
          this.schemaCache = null;
          await this.loadAgentConfig();
          await this.loadUsersPage();
          await this.refreshOnboardingStatus();
          this.onboardingWizard.step = 6;
          this.showToast('Tool and channel settings saved.', 'success');
          return;
        }

        if (this.onboardingWizard.step === 6) {
          this.onboardingWizard.done = true;
          await this.refreshOnboardingStatus();
          this.showToast('Setup complete! Configure your agent settings now.', 'success');
          window.location.hash = '#/config/agent';
        }
      } catch (error) {
        this.onboardingWizard.error = error.message;
      } finally {
        this.onboardingWizard.busy = false;
      }
    },

    showToast(message, kind = 'info') {
      this.toast.message = message;
      this.toast.kind = kind;
      this.toast.visible = true;
      window.clearTimeout(this._toastTimer);
      this._toastTimer = window.setTimeout(() => {
        this.toast.visible = false;
      }, 4000);
    },
  };
}

window.app = app;
