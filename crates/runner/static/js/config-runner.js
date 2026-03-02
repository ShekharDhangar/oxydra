/**
 * RunnerConfigEditor — Structured, section-based editor for runner configuration.
 *
 * Replaces the generic field-by-field editor with purpose-built sections,
 * proper input widgets (dropdowns for sandbox tier, auth mode, etc.),
 * conditional visibility for auth token fields, and a users-page link.
 *
 * Exposed as window.RunnerConfigEditor.
 */
window.RunnerConfigEditor = (function () {
  'use strict';

  var SECRET_SENTINEL = '__UNCHANGED__';

  // ---------------------------------------------------------------------------
  // DOM helpers
  // ---------------------------------------------------------------------------

  function el(tag, className) {
    var node = document.createElement(tag);
    if (className) node.className = className;
    return node;
  }

  // ---------------------------------------------------------------------------
  // Resolve / set a dotted path from a nested object
  // ---------------------------------------------------------------------------

  function resolveValue(path, obj) {
    if (!obj || typeof obj !== 'object') return undefined;
    var parts = path.split('.');
    var cur = obj;
    for (var i = 0; i < parts.length; i++) {
      if (cur == null || typeof cur !== 'object') return undefined;
      cur = cur[parts[i]];
    }
    return cur;
  }

  function setNestedValue(obj, path, value) {
    var parts = path.split('.');
    var cur = obj;
    for (var i = 0; i < parts.length - 1; i++) {
      if (cur[parts[i]] == null || typeof cur[parts[i]] !== 'object') {
        cur[parts[i]] = {};
      }
      cur = cur[parts[i]];
    }
    cur[parts[parts.length - 1]] = value;
  }

  // ---------------------------------------------------------------------------
  // Main render function
  // ---------------------------------------------------------------------------

  /**
   * Render the full runner config editor into a container element.
   *
   * @param {HTMLElement}  container   DOM element to render into.
   * @param {Object}       opts
   * @param {Object}       opts.schema         Schema for 'runner' config type.
   * @param {Object}       opts.config         Current runner config values (nested object).
   * @param {Object}       opts.dynamicSources Dynamic sources from schema endpoint.
   * @param {Function}     opts.onSave         Called with (patch) when user saves.
   * @param {Function}     opts.showToast      Called with (message, kind).
   * @param {boolean}      opts.fileExists     Whether the runner.toml file exists.
   * @param {string}       opts.filePath       Path to the runner.toml file.
   * @returns {Object}     Editor instance with { destroy, getPatch, hasChanges }
   */
  function render(container, opts) {
    container.innerHTML = '';

    var schema = opts.schema;
    var config = opts.config || {};
    var dynamicSources = opts.dynamicSources || {};

    // Track all changes
    var changes = {};          // path → newValue (for standard fields)

    // Track rendered sections and their widgets
    var sectionInstances = {};

    // Track the last rendered group for inserting group headers
    var lastGroup = null;

    // ── Empty state banner ──────────────────────────────────────

    if (opts.fileExists === false) {
      var banner = el('div', 'sr-empty-state-banner');
      banner.setAttribute('role', 'status');
      var bannerIcon = el('span', 'sr-empty-state-icon');
      bannerIcon.textContent = 'ℹ️';
      var bannerText = el('div', 'sr-empty-state-text');
      var bannerTitle = el('strong');
      bannerTitle.textContent = 'No configuration file found';
      var bannerDesc = el('p');
      bannerDesc.textContent = 'The runner config file does not exist yet. ' +
        'All fields below show their default values. ' +
        'Edit and save to create the file.';
      if (opts.filePath) {
        var bannerPath = el('p', 'sr-empty-state-path');
        bannerPath.textContent = 'File will be created at: ' + opts.filePath;
        bannerText.appendChild(bannerTitle);
        bannerText.appendChild(bannerDesc);
        bannerText.appendChild(bannerPath);
      } else {
        bannerText.appendChild(bannerTitle);
        bannerText.appendChild(bannerDesc);
      }
      banner.appendChild(bannerIcon);
      banner.appendChild(bannerText);
      container.appendChild(banner);
    }

    // ── Render each section ─────────────────────────────────────

    (schema.sections || []).forEach(function (sectionSchema) {
      if (sectionSchema.id === 'web') {
        // Special web section with conditional auth fields and loopback warning
        renderWebSection(container, sectionSchema, config, opts);
      } else {
        // Standard section
        renderStandardSection(container, sectionSchema, config, opts);
      }
    });

    // ── Users link section ──────────────────────────────────────

    renderUsersLinkSection(container);

    // ── Insert group header if needed ───────────────────────────

    function insertGroupHeader(parent, sectionSchema) {
      var group = sectionSchema.group;
      if (group && group !== lastGroup) {
        lastGroup = group;
        if (sectionSchema.group_label) {
          var groupHeader = el('div', 'sr-group-header');
          var groupTitle = el('span', 'sr-group-title');
          groupTitle.textContent = sectionSchema.group_label;
          groupHeader.appendChild(groupTitle);
          if (sectionSchema.group_description) {
            var groupDesc = el('span', 'sr-group-description');
            groupDesc.textContent = sectionSchema.group_description;
            groupHeader.appendChild(groupDesc);
          }
          parent.appendChild(groupHeader);
        }
      }
    }

    // ── Standard section rendering ──────────────────────────────

    function renderStandardSection(parent, sectionSchema, configValues, renderOpts) {
      insertGroupHeader(parent, sectionSchema);
      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
        onChange: function (path, newValue) {
          changes[path] = newValue;
        },
        startExpanded: shouldStartExpanded(sectionSchema),
        idPrefix: sectionSchema.id,
      });

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);
    }

    // ── Web section with conditional auth fields ────────────────

    function renderWebSection(parent, sectionSchema, configValues, renderOpts) {
      insertGroupHeader(parent, sectionSchema);

      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
        onChange: function (path, newValue) {
          changes[path] = newValue;
          // Handle conditional visibility for auth fields
          if (path === 'web.auth_mode') {
            updateAuthFieldVisibility(newValue);
          }
          // Handle loopback warning for bind address
          if (path === 'web.bind' || path === 'web.auth_mode') {
            updateLoopbackWarning();
          }
        },
        startExpanded: true,
        idPrefix: sectionSchema.id,
      });

      sectionInstances[sectionSchema.id] = sectionResult;

      // Insert loopback warning banner into the section body
      var body = sectionResult.element.querySelector('.sr-body');
      if (body) {
        var warning = buildLoopbackWarning(configValues);
        body.appendChild(warning);
      }

      parent.appendChild(sectionResult.element);

      // Set initial auth field visibility
      var currentAuthMode = resolveValue('web.auth_mode', configValues) || 'disabled';
      updateAuthFieldVisibility(currentAuthMode);
    }

    // ── Loopback warning ────────────────────────────────────────

    var loopbackWarningEl = null;

    function buildLoopbackWarning(configValues) {
      loopbackWarningEl = el('div', 'runner-loopback-warning');
      loopbackWarningEl.setAttribute('role', 'alert');

      var icon = el('span', 'runner-loopback-warning-icon');
      icon.textContent = '⚠';
      loopbackWarningEl.appendChild(icon);

      var text = el('span');
      text.textContent = 'Warning: Authentication is disabled and bind address is not loopback. ' +
        'The web configurator will be accessible without authentication from any host that can reach this address.';
      loopbackWarningEl.appendChild(text);

      // Set initial visibility
      var bind = resolveValue('web.bind', configValues) || '127.0.0.1:9400';
      var authMode = resolveValue('web.auth_mode', configValues) || 'disabled';
      loopbackWarningEl.style.display = shouldShowLoopbackWarning(bind, authMode) ? '' : 'none';

      return loopbackWarningEl;
    }

    function shouldShowLoopbackWarning(bind, authMode) {
      if (authMode !== 'disabled') return false;
      if (!bind) return false;
      var host = bind.split(':')[0] || '';
      var loopbackHosts = ['127.0.0.1', 'localhost', '::1', '[::1]'];
      return loopbackHosts.indexOf(host) === -1;
    }

    function updateLoopbackWarning() {
      if (!loopbackWarningEl) return;
      var bind = changes['web.bind'] !== undefined
        ? changes['web.bind']
        : (resolveValue('web.bind', config) || '127.0.0.1:9400');
      var authMode = changes['web.auth_mode'] !== undefined
        ? changes['web.auth_mode']
        : (resolveValue('web.auth_mode', config) || 'disabled');
      loopbackWarningEl.style.display = shouldShowLoopbackWarning(bind, authMode) ? '' : 'none';
    }

    // ── Auth field conditional visibility ────────────────────────

    function updateAuthFieldVisibility(authMode) {
      var webSection = sectionInstances['web'];
      if (!webSection || !webSection.widgets) return;

      var tokenFields = ['web.auth_token_env', 'web.auth_token'];
      var showToken = (authMode === 'token');

      tokenFields.forEach(function (path) {
        var widget = webSection.widgets[path];
        if (widget && widget.element) {
          widget.element.style.display = showToken ? '' : 'none';
        }
      });
    }

    // ── Users link section ──────────────────────────────────────

    function renderUsersLinkSection(parent) {
      var card = el('div', 'sr-section');

      var header = el('div', 'sr-header');
      header.setAttribute('role', 'button');
      header.setAttribute('tabindex', '0');
      header.setAttribute('aria-expanded', 'true');

      var headerLeft = el('div', 'sr-header-left');
      var chevron = el('span', 'sr-chevron');
      chevron.textContent = '▾';
      headerLeft.appendChild(chevron);

      var titleGroup = el('div', 'sr-title-group');
      var title = el('span', 'sr-title');
      title.textContent = 'Users';
      titleGroup.appendChild(title);

      var desc = el('span', 'sr-description');
      desc.textContent = 'User registrations and per-user configuration';
      titleGroup.appendChild(desc);

      headerLeft.appendChild(titleGroup);
      header.appendChild(headerLeft);
      card.appendChild(header);

      var body = el('div', 'sr-body');

      var linkCard = el('div', 'runner-users-link-card');
      var linkText = el('p');
      linkText.textContent = 'Users are managed on the dedicated Users page.';
      linkCard.appendChild(linkText);

      var linkBtn = el('a', 'btn btn-primary btn-sm');
      linkBtn.href = '#/config/users';
      linkBtn.textContent = '→ Go to Users Page';
      linkCard.appendChild(linkBtn);

      body.appendChild(linkCard);

      var isExpanded = true;
      header.addEventListener('click', function () {
        isExpanded = !isExpanded;
        body.style.display = isExpanded ? '' : 'none';
        chevron.textContent = isExpanded ? '▾' : '▸';
        header.setAttribute('aria-expanded', String(isExpanded));
      });
      header.addEventListener('keydown', function (e) {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          isExpanded = !isExpanded;
          body.style.display = isExpanded ? '' : 'none';
          chevron.textContent = isExpanded ? '▾' : '▸';
          header.setAttribute('aria-expanded', String(isExpanded));
        }
      });

      card.appendChild(body);
      parent.appendChild(card);
    }

    // ── Helpers ─────────────────────────────────────────────────

    function shouldStartExpanded(sectionSchema) {
      var expandedIds = ['general', 'web'];
      return expandedIds.indexOf(sectionSchema.id) !== -1;
    }

    function escapeHtml(str) {
      var div = document.createElement('div');
      div.appendChild(document.createTextNode(str));
      return div.innerHTML;
    }

    // ── Build the save patch ────────────────────────────────────

    function buildPatch() {
      var patch = {};
      var hasChanges = false;

      // 1. Standard field changes
      Object.keys(changes).forEach(function (path) {
        setNestedValue(patch, path, changes[path]);
        hasChanges = true;
      });

      // 2. Handle secrets: any secret field that wasn't touched gets __UNCHANGED__
      (schema.sections || []).forEach(function (section) {
        addUnchangedSecrets(section, patch);
      });

      return { patch: patch, hasChanges: hasChanges };
    }

    function addUnchangedSecrets(section, patch) {
      (section.fields || []).forEach(function (f) {
        if (f.input_type === 'secret') {
          var widget = sectionInstances[section.id] &&
                       sectionInstances[section.id].widgets &&
                       sectionInstances[section.id].widgets[f.path];
          if (widget) {
            var val = widget.getValue();
            if (val === SECRET_SENTINEL) {
              setNestedValue(patch, f.path, SECRET_SENTINEL);
            }
          }
        }
      });
      (section.subsections || []).forEach(function (sub) {
        addUnchangedSecrets(sub, patch);
      });
    }

    function hasAnyChanges() {
      return Object.keys(changes).length > 0;
    }

    // ── Public interface ────────────────────────────────────────

    return {
      destroy: function () {
        container.innerHTML = '';
      },
      getPatch: buildPatch,
      hasChanges: hasAnyChanges,
    };
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  return {
    render: render,
  };
})();
