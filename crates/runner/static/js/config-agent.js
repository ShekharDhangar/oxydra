/**
 * AgentConfigEditor — Structured, section-based editor for agent configuration.
 *
 * Replaces the generic field-by-field editor with purpose-built sections,
 * proper input widgets, collection editors for providers/agents, and
 * catalog integration.
 *
 * Supports:
 *  - Group headers for organizing related sections under common headings.
 *  - Provider→model picker filter wiring for the selection section.
 *
 * Exposed as window.AgentConfigEditor.
 */
window.AgentConfigEditor = (function () {
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
  // Resolve a dotted path from a nested object
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
   * Render the full agent config editor into a container element.
   *
   * @param {HTMLElement}  container   DOM element to render into.
   * @param {Object}       opts
   * @param {Object}       opts.schema        Schema for 'agent' config type.
   * @param {Object}       opts.config        Current agent config values (nested object).
   * @param {Object}       opts.dynamicSources  Dynamic sources from schema endpoint.
   * @param {Array}        opts.catalog        Catalog providers array.
   * @param {Object}       opts.catalogStatus  Catalog status info.
   * @param {Function}     opts.onSave         Called with (patch) when user saves.
   * @param {Function}     opts.onRefreshCatalog  Called to refresh catalog.
   * @param {Function}     opts.showToast      Called with (message, kind).
   * @param {boolean}      opts.fileExists     Whether the agent.toml file exists.
   * @param {string}       opts.filePath       Path to the agent.toml file.
   * @returns {Object}     Editor instance with { destroy, getPatch }
   */
  function render(container, opts) {
    container.innerHTML = '';

    var schema = opts.schema;
    var config = opts.config || {};
    var dynamicSources = opts.dynamicSources || {};
    var catalog = opts.catalog || [];
    var catalogStatus = opts.catalogStatus || {};

    // Track all changes
    var changes = {};          // path → newValue (for standard fields)
    var collectionChanges = {};  // sectionId → patch object (for collections)
    var disabledSections = {};   // sectionId → true (toggled off optional sections)
    var enabledSections = {};    // sectionId → true (toggled on optional sections that were empty)
    var originalSectionStates = {}; // sectionId → boolean (initial state)

    // Track rendered sections and their widgets
    var sectionInstances = {};
    var collectionInstances = {};

    // Track the last rendered group for inserting group headers
    var lastGroup = null;

    // ── Empty state banner ──────────────────────────────────────

    if (!opts.fileExists) {
      var banner = el('div', 'sr-empty-state-banner');
      banner.setAttribute('role', 'status');
      var bannerIcon = el('span', 'sr-empty-state-icon');
      bannerIcon.textContent = 'ℹ️';
      var bannerText = el('div', 'sr-empty-state-text');
      var bannerTitle = el('strong');
      bannerTitle.textContent = 'No configuration file found';
      var bannerDesc = el('p');
      bannerDesc.textContent = 'The agent config file does not exist yet. ' +
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
      // Determine initial enabled state for optional sections
      if (sectionSchema.optional_section) {
        var hasValues = sectionHasValues(sectionSchema, config);
        originalSectionStates[sectionSchema.id] = hasValues;
      }

      if (sectionSchema.collection) {
        // Collection section (providers, agents)
        renderCollectionSection(container, sectionSchema, config, opts);
      } else if (sectionSchema.id === 'catalog') {
        // Special catalog section with status card and refresh button
        renderCatalogSection(container, sectionSchema, config, opts);
      } else if (sectionSchema.id === 'selection') {
        // Selection section with provider→model wiring
        renderSelectionSection(container, sectionSchema, config, opts);
      } else {
        // Standard or optional section
        renderStandardSection(container, sectionSchema, config, opts);
      }
    });

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
        catalog: renderOpts.catalog,
        onChange: function (path, newValue) {
          changes[path] = newValue;
        },
        onToggleSection: function (sectionId, enabled) {
          if (enabled) {
            enabledSections[sectionId] = true;
            delete disabledSections[sectionId];
          } else {
            disabledSections[sectionId] = true;
            delete enabledSections[sectionId];
          }
        },
        startExpanded: shouldStartExpanded(sectionSchema),
        idPrefix: sectionSchema.id,
      });

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);
    }

    // ── Selection section with provider→model wiring ────────────

    function renderSelectionSection(parent, sectionSchema, configValues, renderOpts) {
      insertGroupHeader(parent, sectionSchema);

      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
        catalog: renderOpts.catalog,
        onChange: function (path, newValue) {
          changes[path] = newValue;

          // Wire provider → model picker filter
          if (path === 'selection.provider' && sectionResult.widgets['selection.model']) {
            var modelWidget = sectionResult.widgets['selection.model'];
            var catalogProvider = resolveProviderCatalogId(newValue, configValues, renderOpts);
            if (modelWidget.setProviderFilter) {
              modelWidget.setProviderFilter(catalogProvider);
            }
          }
        },
        startExpanded: true,
        idPrefix: sectionSchema.id,
      });

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);

      // Set initial provider filter on the model picker
      var currentProvider = resolveValue('selection.provider', configValues);
      if (currentProvider && sectionResult.widgets['selection.model']) {
        var modelWidget = sectionResult.widgets['selection.model'];
        var catalogProvider = resolveProviderCatalogId(currentProvider, configValues, renderOpts);
        if (modelWidget.setProviderFilter) {
          modelWidget.setProviderFilter(catalogProvider);
        }
      }
    }

    /**
     * Resolve a provider registry name to its catalog provider ID.
     * Looks up the provider in the current config to find catalog_provider
     * or infers it from provider_type.
     */
    function resolveProviderCatalogId(providerName, configValues, renderOpts) {
      if (!providerName) return '';
      var registry = (configValues.providers && configValues.providers.registry) || {};
      var entry = registry[providerName];
      if (!entry) return providerName; // Best guess: use the name itself

      // Use explicit catalog_provider if set
      if (entry.catalog_provider) return entry.catalog_provider;

      // Infer from provider_type
      var typeMapping = {
        'openai': 'openai',
        'anthropic': 'anthropic',
        'gemini': 'google',
        'openai_responses': 'openai',
      };
      return typeMapping[entry.provider_type] || providerName;
    }

    // ── Collection section rendering ────────────────────────────

    function renderCollectionSection(parent, sectionSchema, configValues, renderOpts) {
      insertGroupHeader(parent, sectionSchema);

      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
        catalog: renderOpts.catalog,
        onChange: function () {},
        startExpanded: shouldStartExpanded(sectionSchema),
        idPrefix: sectionSchema.id,
      });

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);

      // Now render the collection editor into the placeholder
      if (sectionResult.collectionPlaceholder) {
        var collectionData = getCollectionData(sectionSchema.id, configValues);
        var collectionEditor = window.CollectionEditor.renderCollection(
          sectionResult.collectionPlaceholder,
          sectionSchema,
          collectionData,
          {
            dynamicSources: renderOpts.dynamicSources,
            catalog: renderOpts.catalog,
            onChange: function (patch) {
              collectionChanges[sectionSchema.id] = patch;
            },
          }
        );
        collectionInstances[sectionSchema.id] = collectionEditor;
      }
    }

    // ── Catalog section with status card ─────────────────────────

    function renderCatalogSection(parent, sectionSchema, configValues, renderOpts) {
      insertGroupHeader(parent, sectionSchema);

      // First render the standard fields
      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
        catalog: renderOpts.catalog,
        onChange: function (path, newValue) {
          changes[path] = newValue;
        },
        startExpanded: false,
        idPrefix: sectionSchema.id,
      });

      // Add catalog status card inside the section body
      var body = sectionResult.element.querySelector('.sr-body');
      if (body) {
        var statusCard = buildCatalogStatusCard(catalogStatus, renderOpts);
        body.appendChild(statusCard);
      }

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);
    }

    function buildCatalogStatusCard(status, renderOpts) {
      var card = el('div', 'catalog-status-card');

      var header = el('div', 'catalog-status-header');
      header.textContent = 'Catalog Status';
      card.appendChild(header);

      var info = el('div', 'catalog-status-info');

      var sourceLine = el('p', 'catalog-status-line');
      sourceLine.innerHTML = '<strong>Source:</strong> ' + escapeHtml(status.source || 'Unknown');
      info.appendChild(sourceLine);

      if (status.last_modified) {
        var modLine = el('p', 'catalog-status-line');
        var ts = Number(status.last_modified);
        var dateStr = ts ? new Date(ts * 1000).toLocaleString() : status.last_modified;
        modLine.innerHTML = '<strong>Last Modified:</strong> ' + escapeHtml(dateStr);
        info.appendChild(modLine);
      }

      var provLine = el('p', 'catalog-status-line');
      provLine.innerHTML = '<strong>Providers:</strong> ' + (status.provider_count || 0) +
        ' &nbsp; <strong>Models:</strong> ' + (status.model_count || 0);
      info.appendChild(provLine);

      card.appendChild(info);

      var actions = el('div', 'catalog-status-actions');
      var refreshBtn = el('button', 'btn btn-muted btn-sm');
      refreshBtn.type = 'button';
      refreshBtn.textContent = '↻ Refresh Catalog';
      var refreshing = false;

      refreshBtn.addEventListener('click', function () {
        if (refreshing) return;
        refreshing = true;
        refreshBtn.disabled = true;
        refreshBtn.textContent = 'Refreshing…';

        if (renderOpts.onRefreshCatalog) {
          renderOpts.onRefreshCatalog()
            .then(function (result) {
              if (result) {
                provLine.innerHTML = '<strong>Providers:</strong> ' + (result.provider_count || 0) +
                  ' &nbsp; <strong>Models:</strong> ' + (result.model_count || 0);
                sourceLine.innerHTML = '<strong>Source:</strong> ' + escapeHtml(result.source || 'Refreshed');
              }
              refreshBtn.textContent = '✓ Refreshed';
              setTimeout(function () {
                refreshBtn.textContent = '↻ Refresh Catalog';
                refreshBtn.disabled = false;
                refreshing = false;
              }, 2000);
            })
            .catch(function () {
              refreshBtn.textContent = '↻ Refresh Catalog';
              refreshBtn.disabled = false;
              refreshing = false;
            });
        }
      });

      actions.appendChild(refreshBtn);
      card.appendChild(actions);

      return card;
    }

    // ── Helpers ─────────────────────────────────────────────────

    function getCollectionData(sectionId, configValues) {
      if (sectionId === 'providers') {
        return (configValues.providers && configValues.providers.registry) || {};
      }
      if (sectionId === 'agents') {
        return configValues.agents || {};
      }
      return {};
    }

    function shouldStartExpanded(sectionSchema) {
      // Start selection and general expanded, others collapsed
      var expandedIds = ['general', 'selection'];
      return expandedIds.indexOf(sectionSchema.id) !== -1;
    }

    function sectionHasValues(sectionSchema, configValues) {
      return sectionSchema.fields.some(function (f) {
        var v = resolveValue(f.path, configValues);
        return v !== undefined && v !== null;
      });
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

      // 2. Collection changes (providers, agents)
      if (collectionChanges.providers) {
        if (!patch.providers) patch.providers = {};
        patch.providers.registry = collectionChanges.providers;
        hasChanges = true;
      }
      if (collectionChanges.agents) {
        patch.agents = collectionChanges.agents;
        hasChanges = true;
      }

      // 3. Disabled optional sections → send null to remove them
      Object.keys(disabledSections).forEach(function (sectionId) {
        // Only send null if the section was originally present
        if (originalSectionStates[sectionId]) {
          setNestedValue(patch, sectionId, null);
          hasChanges = true;
        }
      });

      // 4. Enabled optional sections → send section with defaults
      Object.keys(enabledSections).forEach(function (sectionId) {
        // Only send if the section was originally absent
        if (!originalSectionStates[sectionId]) {
          var sectionSchema = findSectionById(sectionId);
          if (sectionSchema) {
            var defaults = {};
            sectionSchema.fields.forEach(function (f) {
              if (f.default != null) {
                setNestedValue(defaults, f.path, JSON.parse(JSON.stringify(f.default)));
              }
            });
            // Also include any user changes for this section
            Object.keys(changes).forEach(function (path) {
              if (path.indexOf(sectionId + '.') === 0 || path === sectionId) {
                setNestedValue(defaults, path, changes[path]);
              }
            });
            setNestedValue(patch, sectionId, resolveValue(sectionId, defaults) || {});
            hasChanges = true;
          }
        }
      });

      // Handle secrets: any secret field that wasn't touched gets __UNCHANGED__
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

    function findSectionById(id) {
      return (schema.sections || []).find(function (s) { return s.id === id; });
    }

    function hasAnyChanges() {
      if (Object.keys(changes).length > 0) return true;
      if (Object.keys(collectionChanges).length > 0) return true;
      if (Object.keys(disabledSections).length > 0) {
        for (var key in disabledSections) {
          if (originalSectionStates[key]) return true;
        }
      }
      if (Object.keys(enabledSections).length > 0) {
        for (var key2 in enabledSections) {
          if (!originalSectionStates[key2]) return true;
        }
      }
      return false;
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
