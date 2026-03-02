/**
 * UserConfigEditor — Structured, section-based editor for per-user configuration.
 *
 * Replaces the generic field-by-field editor with purpose-built sections:
 *  - Mounts: optional text fields for shared/tmp/vault paths
 *  - Resources: optional number fields for vCPUs/memory/processes
 *  - Credential References: key-value map collection with secret masking
 *  - Behavior Overrides: optional dropdown for sandbox_tier, tri-state booleans
 *  - Telegram Channel: optional section with sender bindings (array collection)
 *
 * Exposed as window.UserConfigEditor.
 */
window.UserConfigEditor = (function () {
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
   * Render the full user config editor into a container element.
   *
   * @param {HTMLElement}  container   DOM element to render into.
   * @param {Object}       opts
   * @param {Object}       opts.schema         Schema for 'user' config type.
   * @param {Object}       opts.config         Current user config values (nested object).
   * @param {Object}       opts.dynamicSources Dynamic sources from schema endpoint.
   * @param {Function}     opts.showToast      Called with (message, kind).
   * @param {boolean}      opts.fileExists     Whether the user TOML file exists.
   * @param {string}       opts.filePath       Path to the user TOML file.
   * @returns {Object}     Editor instance with { destroy, getPatch, hasChanges }
   */
  function render(container, opts) {
    container.innerHTML = '';

    var schema = opts.schema;
    var config = opts.config || {};
    var dynamicSources = opts.dynamicSources || {};

    // Track all changes
    var changes = {};          // path → newValue (for standard fields)
    var collectionChanges = {};  // sectionId → patch object (for collections)
    var disabledSections = {};   // sectionId → true (toggled off optional sections)
    var enabledSections = {};    // sectionId → true (toggled on optional sections that were empty)
    var originalSectionStates = {}; // sectionId → boolean (initial state)

    // Track rendered sections and their widgets
    var sectionInstances = {};
    var collectionInstances = {};

    // ── Render each section ─────────────────────────────────────

    (schema.sections || []).forEach(function (sectionSchema) {
      // Determine initial enabled state for optional sections
      if (sectionSchema.optional_section) {
        var hasValues = sectionHasValues(sectionSchema, config);
        originalSectionStates[sectionSchema.id] = hasValues;
      }

      if (sectionSchema.collection) {
        // Collection section (credential_refs)
        renderCollectionSection(container, sectionSchema, config, opts);
      } else if (sectionSchema.id === 'channels.telegram') {
        // Telegram section with sender bindings
        renderTelegramSection(container, sectionSchema, config, opts);
      } else {
        // Standard or optional section
        renderStandardSection(container, sectionSchema, config, opts);
      }
    });

    // ── Standard section rendering ──────────────────────────────

    function renderStandardSection(parent, sectionSchema, configValues, renderOpts) {
      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
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

    // ── Collection section rendering (credential_refs) ──────────

    function renderCollectionSection(parent, sectionSchema, configValues, renderOpts) {
      var sectionResult = window.SectionRenderer.renderSection(sectionSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
        onChange: function () {},
        startExpanded: shouldStartExpanded(sectionSchema),
        idPrefix: sectionSchema.id,
      });

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);

      // Render the collection editor into the placeholder
      if (sectionResult.collectionPlaceholder) {
        var collectionData = getCollectionData(sectionSchema.id, configValues);
        var collectionEditor = window.CollectionEditor.renderCollection(
          sectionResult.collectionPlaceholder,
          sectionSchema,
          collectionData,
          {
            dynamicSources: renderOpts.dynamicSources,
            onChange: function (patch) {
              collectionChanges[sectionSchema.id] = patch;
            },
          }
        );
        collectionInstances[sectionSchema.id] = collectionEditor;
      }
    }

    // ── Telegram section with sender bindings ───────────────────

    function renderTelegramSection(parent, sectionSchema, configValues, renderOpts) {
      // Separate senders from other fields
      var regularFields = [];
      var sendersField = null;
      sectionSchema.fields.forEach(function (f) {
        if (f.path === 'channels.telegram.senders') {
          sendersField = f;
        } else {
          regularFields.push(f);
        }
      });

      // Build a modified section schema without senders
      var modifiedSchema = {
        id: sectionSchema.id,
        label: sectionSchema.label,
        description: sectionSchema.description,
        fields: regularFields,
        subsections: sectionSchema.subsections || [],
        collection: null,
        optional_section: sectionSchema.optional_section,
      };

      var sectionResult = window.SectionRenderer.renderSection(modifiedSchema, configValues, {
        dynamicSources: renderOpts.dynamicSources,
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

      // Add sender bindings editor inside the section body
      var body = sectionResult.element.querySelector('.sr-body');
      if (body && sendersField) {
        var sendersContainer = el('div', 'user-senders-section');
        var sendersHeader = el('h4', 'user-senders-header');
        sendersHeader.textContent = 'Sender Bindings';
        sendersContainer.appendChild(sendersHeader);

        var sendersDesc = el('p', 'fr-description');
        sendersDesc.textContent = 'Authorized sender bindings (platform IDs and display names)';
        sendersContainer.appendChild(sendersDesc);

        var sendersEntriesContainer = el('div', 'user-senders-entries');
        sendersContainer.appendChild(sendersEntriesContainer);

        // Get current senders array
        var currentSenders = resolveValue('channels.telegram.senders', configValues) || [];

        // Render existing sender cards
        var senderCards = [];

        function renderSenderCard(sender, index) {
          var card = el('div', 'ce-card');
          var expanded = true;

          // Header
          var header = el('div', 'ce-card-header');
          var headerLeft = el('div', 'ce-card-header-left');

          var chevron = el('span', 'sr-chevron');
          chevron.textContent = expanded ? '▾' : '▸';
          headerLeft.appendChild(chevron);

          var keyLabel = el('span', 'ce-card-key');
          keyLabel.textContent = sender.display_name || ('Sender ' + (index + 1));
          headerLeft.appendChild(keyLabel);

          if (sender.platform_ids && sender.platform_ids.length > 0) {
            var idBadge = el('span', 'badge badge-muted ce-type-badge');
            idBadge.textContent = sender.platform_ids.length + ' ID(s)';
            headerLeft.appendChild(idBadge);
          }

          header.appendChild(headerLeft);

          var headerActions = el('div', 'ce-card-actions');
          var deleteBtn = el('button', 'btn btn-danger btn-sm');
          deleteBtn.type = 'button';
          deleteBtn.textContent = 'Delete';
          deleteBtn.addEventListener('click', function (e) {
            e.stopPropagation();
            if (!window.confirm('Delete this sender binding?')) return;
            var idx = senderCards.indexOf(cardState);
            if (idx !== -1) {
              senderCards.splice(idx, 1);
              card.remove();
              fireSendersChange();
            }
          });
          headerActions.appendChild(deleteBtn);
          header.appendChild(headerActions);

          header.addEventListener('click', function () {
            expanded = !expanded;
            cardBody.style.display = expanded ? '' : 'none';
            chevron.textContent = expanded ? '▾' : '▸';
          });

          card.appendChild(header);

          // Body
          var cardBody = el('div', 'ce-card-body');

          // Platform IDs as tag list
          var platformIdsWrap = el('div', 'fr-field');
          var platformIdsLabel = el('label', 'fr-label');
          platformIdsLabel.textContent = 'Platform IDs';
          platformIdsWrap.appendChild(platformIdsLabel);

          var platformIdsDesc = el('p', 'fr-description');
          platformIdsDesc.textContent = 'Telegram user ID strings for this sender';
          platformIdsWrap.appendChild(platformIdsDesc);

          var currentIds = (sender.platform_ids || []).slice();
          var tagListWidget = window.FormRenderer.renderField(
            {
              path: 'platform_ids',
              label: 'Platform IDs',
              description: 'Telegram user ID strings for this sender',
              input_type: 'tag_list',
              required: false,
              nullable: false,
            },
            currentIds,
            {
              dynamicSources: renderOpts.dynamicSources,
              onChange: function (path, newValue) {
                cardState.values.platform_ids = newValue;
                fireSendersChange();
              },
              idPrefix: 'sender-' + index,
            }
          );
          // Use only the widget element (label is already part of the rendered field)
          cardBody.appendChild(tagListWidget.element);

          // Display name
          var displayNameWidget = window.FormRenderer.renderField(
            {
              path: 'display_name',
              label: 'Display Name',
              description: 'Human-readable name for logging',
              input_type: 'text',
              required: false,
              nullable: true,
            },
            sender.display_name || '',
            {
              dynamicSources: renderOpts.dynamicSources,
              onChange: function (path, newValue) {
                cardState.values.display_name = newValue;
                // Update the card header label
                keyLabel.textContent = newValue || ('Sender ' + (senderCards.indexOf(cardState) + 1));
                fireSendersChange();
              },
              idPrefix: 'sender-' + index,
            }
          );
          cardBody.appendChild(displayNameWidget.element);

          card.appendChild(cardBody);

          var cardState = {
            element: card,
            values: {
              platform_ids: currentIds,
              display_name: sender.display_name || null,
            },
          };

          return cardState;
        }

        currentSenders.forEach(function (sender, idx) {
          var cardState = renderSenderCard(sender, idx);
          senderCards.push(cardState);
          sendersEntriesContainer.appendChild(cardState.element);
        });

        // Add sender button
        var addRow = el('div', 'ce-add-row');
        var addBtn = el('button', 'btn btn-primary btn-sm');
        addBtn.type = 'button';
        addBtn.textContent = 'Add Sender';
        addBtn.addEventListener('click', function () {
          var newSender = { platform_ids: [], display_name: null };
          var cardState = renderSenderCard(newSender, senderCards.length);
          senderCards.push(cardState);
          sendersEntriesContainer.appendChild(cardState.element);
          fireSendersChange();
        });
        addRow.appendChild(addBtn);
        sendersContainer.appendChild(addRow);

        function fireSendersChange() {
          var sendersArray = senderCards.map(function (s) {
            var result = { platform_ids: s.values.platform_ids || [] };
            if (s.values.display_name) {
              result.display_name = s.values.display_name;
            }
            return result;
          });
          changes['channels.telegram.senders'] = sendersArray;
        }

        body.appendChild(sendersContainer);
      }

      sectionInstances[sectionSchema.id] = sectionResult;
      parent.appendChild(sectionResult.element);
    }

    // ── Helpers ─────────────────────────────────────────────────

    function getCollectionData(sectionId, configValues) {
      if (sectionId === 'credential_refs') {
        return configValues.credential_refs || {};
      }
      return {};
    }

    function shouldStartExpanded(sectionSchema) {
      var expandedIds = ['general', 'mounts', 'behavior'];
      return expandedIds.indexOf(sectionSchema.id) !== -1;
    }

    function sectionHasValues(sectionSchema, configValues) {
      return sectionSchema.fields.some(function (f) {
        var v = resolveValue(f.path, configValues);
        return v !== undefined && v !== null;
      });
    }

    function findSectionById(id) {
      return (schema.sections || []).find(function (s) { return s.id === id; });
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

      // 2. Collection changes (credential_refs)
      if (collectionChanges.credential_refs) {
        patch.credential_refs = collectionChanges.credential_refs;
        hasChanges = true;
      }

      // 3. Disabled optional sections → send null to remove them
      Object.keys(disabledSections).forEach(function (sectionId) {
        if (originalSectionStates[sectionId]) {
          setNestedValue(patch, sectionId, null);
          hasChanges = true;
        }
      });

      // 4. Enabled optional sections → send section with defaults
      Object.keys(enabledSections).forEach(function (sectionId) {
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

      // 5. Handle secrets: any secret field that wasn't touched gets __UNCHANGED__
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
