(() => {
  "use strict";

  const incoming = globalThis.__ZNNZ_CLIENT_CONFIG__ || {};
  const existingUnlock = globalThis.__ZNNZ_MODEL_UNLOCK__;
  if (existingUnlock && existingUnlock.version === incoming.version) {
    existingUnlock.configure(incoming);
    return;
  }

  const state = {
    config: { ...incoming },
    catalog: { models: [] },
    names: [],
    metadata: new Map(),
    loadedAt: 0,
    loading: null,
    catalogSource: "none",
    failures: [],
    patchedClients: new WeakSet(),
    patchedStatsigClients: new WeakSet(),
    mcpRequestIds: new Set(),
    patchedClientCount: 0,
    patchedStatsigClientCount: 0,
    patchedModelResponses: 0,
    patchedLabelCount: 0,
    messageHookInstalled: false,
      pickerInteractionHookInstalled: false,
  };

  const logFailure = (where, error) => {
    const item = { where, message: String(error?.message || error), at: Date.now() };
    state.failures.push(item);
    if (state.failures.length > 30) state.failures.shift();
    console.warn("[znnz-client]", where, item.message);
  };

  const unique = (items) => [...new Set(items
    .filter((value) => typeof value === "string" && value.trim())
    .map((value) => value.trim()))];

  const normalizeCatalog = (payload) => {
    const all = Array.isArray(payload?.models) ? payload.models : [];
    const models = all.filter((model) => model && typeof model.slug === "string"
      && model.visibility !== "hide" && model.supported_in_api !== false);
    const metadata = new Map();
    for (const model of models) metadata.set(model.slug, model);
    state.catalog = { ...payload, models };
    state.metadata = metadata;
    state.names = unique(models.map((model) => model.slug));
    state.loadedAt = Date.now();
    return state.catalog;
  };

  const loadCatalog = async (force = false) => {
    if (!force && state.loading) return state.loading;
    if (!force && state.names.length && Date.now() - state.loadedAt < 30_000) return state.catalog;
    const inlineCatalog = state.config.catalog;
    if (inlineCatalog && Array.isArray(inlineCatalog.models)) {
      state.catalogSource = "inline";
      return normalizeCatalog(inlineCatalog);
    }
    const url = state.config.catalogUrl;
    if (!url) throw new Error("catalogUrl and inline catalog are missing");
    state.loading = fetch(url, { cache: "no-store", credentials: "omit" })
      .then((response) => {
        if (!response.ok) throw new Error(`Helper HTTP ${response.status}`);
        return response.json();
      })
      .then((payload) => {
        state.catalogSource = "helper";
        return normalizeCatalog(payload);
      })
      .finally(() => { state.loading = null; });
    return state.loading;
  };

  const modelMetadata = (name) => state.metadata.get(name) || {};

  const normalizedLabel = (value) => String(value || "").trim().replace(/\s+/g, " ").toLowerCase();

  const titleModelPart = (value) => String(value || "")
    .split(/[-_.]+/)
    .filter(Boolean)
    .map((part) => part.length ? `${part[0].toUpperCase()}${part.slice(1)}` : part)
    .join(" ");

  const modelLabelCandidates = (name) => {
    const labels = new Set([name]);
    const model = modelMetadata(name);
    for (const key of ["displayName", "display_name", "name", "title", "label"]) {
      if (typeof model[key] === "string" && model[key].trim()) labels.add(model[key].trim());
    }
    const gpt = /^gpt-(\d+(?:\.\d+)*)(?:-(.+))?$/i.exec(name);
    if (gpt) {
      const suffix = titleModelPart(gpt[2]);
      labels.add(suffix ? `${gpt[1]} ${suffix}` : gpt[1]);
      labels.add(suffix ? `GPT-${gpt[1]} ${suffix}` : `GPT-${gpt[1]}`);
    }
    return [...labels];
  };

  const modelNameForLabel = (label) => {
    const normalized = normalizedLabel(label);
    if (!normalized) return null;
    const exact = state.names.find((name) => normalizedLabel(name) === normalized);
    if (exact) return exact;
    const matches = state.names.filter((name) => modelLabelCandidates(name)
      .some((candidate) => normalizedLabel(candidate) === normalized));
    return matches.length === 1 ? matches[0] : null;
  };

  const modelTextMatch = (container) => {
    if (!container) return null;
    const leaves = [];
    if (!container.children || container.children.length === 0) leaves.push(container);
    if (typeof container.querySelectorAll === "function") {
      for (const element of container.querySelectorAll("*")) {
        if (!element.children || element.children.length === 0) leaves.push(element);
      }
    }
    for (const leaf of leaves) {
      const label = String(leaf.textContent || "").trim();
      const name = modelNameForLabel(label);
      if (name) return { leaf, label, name };
    }
    return null;
  };

  const patchModelText = (container) => {
    const match = modelTextMatch(container);
    if (!match) return null;
    if (match.label !== match.name) {
      match.leaf.textContent = match.name;
      state.patchedLabelCount += 1;
    }
    if (typeof container.getAttribute === "function" && typeof container.setAttribute === "function") {
      const ariaLabel = container.getAttribute("aria-label");
      if (ariaLabel && match.label !== match.name && ariaLabel.endsWith(match.label)) {
        container.setAttribute("aria-label", `${ariaLabel.slice(0, -match.label.length)}${match.name}`);
      }
    }
    return match.name;
  };

  const directMenuItems = (menu) => {
    if (!menu || typeof menu.querySelectorAll !== "function") return [];
    return [...menu.querySelectorAll('[role="menuitem"], [role="menuitemradio"], [role="option"]')]
      .filter((item) => typeof item.closest !== "function" || item.closest('[role="menu"]') === menu);
  };

  const patchModelPickerLabels = () => {
    if (typeof document === "undefined" || !state.names.length) return 0;
    const before = state.patchedLabelCount;
    const submenuIds = new Set();
    for (const item of document.querySelectorAll('[role="menuitem"][aria-haspopup="menu"][aria-controls]')) {
      patchModelText(item);
      const controls = item.getAttribute("aria-controls");
      if (controls) submenuIds.add(controls);
    }

    for (const id of submenuIds) {
      const menu = typeof document.getElementById === "function" ? document.getElementById(id) : null;
      for (const item of directMenuItems(menu)) patchModelText(item);
    }
    return state.patchedLabelCount - before;
  };

  const interactionElement = (target) => {
    if (!target) return null;
    if (typeof target.closest === "function") return target;
    return target.parentElement && typeof target.parentElement.closest === "function"
      ? target.parentElement
      : null;
  };

  const isModelPickerInteraction = (target) => {
    const element = interactionElement(target);
    if (!element) return false;
    if (element.closest('button[data-codex-intelligence-trigger="true"]')) return true;
    const submenuTrigger = element.closest('[role="menuitem"][aria-haspopup="menu"][aria-controls]');
    return Boolean(submenuTrigger);
  };

  let pickerRefreshWindowUntil = 0;
  const schedulePickerLabelRefresh = () => {
    const now = Date.now();
    if (now < pickerRefreshWindowUntil) return false;
    pickerRefreshWindowUntil = now + 100;
    const patch = () => {
      try { patchModelPickerLabels(); } catch (error) { logFailure("model-labels-event", error); }
    };
    if (typeof queueMicrotask === "function") queueMicrotask(patch);
    if (typeof requestAnimationFrame === "function") requestAnimationFrame(patch);
    for (const delay of [0, 16, 40, 80, 150, 300, 600]) setTimeout(patch, delay);
    return true;
  };

  const installPickerInteractionPatch = () => {
    if (window.__ZNNZ_PICKER_INTERACTION_PATCHED__) {
      state.pickerInteractionHookInstalled = true;
      return;
    }
    window.__ZNNZ_PICKER_INTERACTION_PATCHED__ = true;
    state.pickerInteractionHookInstalled = true;
    const handlePointer = (event) => {
      if (isModelPickerInteraction(event?.target)) schedulePickerLabelRefresh();
    };
    const handleKeydown = (event) => {
      if (!["Enter", " ", "Spacebar", "ArrowDown"].includes(String(event?.key || ""))) return;
      if (isModelPickerInteraction(event?.target)) schedulePickerLabelRefresh();
    };
    window.addEventListener("pointerdown", handlePointer, true);
    window.addEventListener("click", handlePointer, true);
    window.addEventListener("keydown", handleKeydown, true);
  };


  const reasoningEfforts = (model) => {
    const raw = model.supported_reasoning_levels
      || model.supportedReasoningLevels
      || model.supported_reasoning_efforts
      || model.supportedReasoningEfforts;
    if (Array.isArray(raw) && raw.length) {
      return raw.map((item) => {
        const effort = typeof item === "string"
          ? item
          : item.reasoning_effort || item.reasoningEffort || item.effort || "medium";
        return {
          reasoningEffort: effort,
          description: typeof item === "object" && item?.description
            ? item.description
            : `${effort} effort`,
        };
      });
    }
    return ["low", "medium", "high", "xhigh", "max", "ultra"].map((reasoningEffort) => ({
      reasoningEffort,
      description: `${reasoningEffort} effort`,
    }));
  };

  const descriptor = (name, template = null) => {
    const model = modelMetadata(name);
    const defaultEffort = model.default_reasoning_level || model.defaultReasoningLevel || "medium";
    const description = state.config.modelDescription || model.description || "From gateway";
    return {
      ...(template && typeof template === "object" ? template : {}),
      model: name,
      id: name,
      slug: name,
      name,
      displayName: name,
      display_name: name,
      description,
      hidden: false,
      isDefault: name === state.config.defaultModel,
      defaultReasoningEffort: defaultEffort,
      // Keep all spellings used by current and older Codex Desktop builds.
      // Some builds read the camelCase field, while newer builds read the
      // snake_case capability field and otherwise fall back to three levels.
      supportedReasoningEfforts: reasoningEfforts(model),
      supported_reasoning_levels: reasoningEfforts(model),
      supported_reasoning_efforts: reasoningEfforts(model),
      visibility: "list",
      supportedInApi: true,
      supported_in_api: true,
    };
  };

  const isModelDescriptorArray = (value, allowEmpty = false) => Array.isArray(value)
    && (allowEmpty || value.length > 0)
    && value.every((item) => item && typeof item === "object" && typeof item.model === "string");

  const patchModelDescriptorArray = (models, allowEmpty = false) => {
    if (!isModelDescriptorArray(models, allowEmpty) || !state.names.length) return false;
    const template = models.find((item) => item && typeof item.model === "string") || null;
    const existing = new Map(models.map((item) => [item.model, item]));
    let changed = false;

    for (const [name, item] of existing) {
      if (!state.metadata.has(name)) continue;
      const next = descriptor(name, item);
      for (const [key, value] of Object.entries(next)) {
        if (item[key] !== value) {
          item[key] = value;
          changed = true;
        }
      }
    }

    for (const name of state.names) {
      if (!existing.has(name)) {
        models.push(descriptor(name, template));
        changed = true;
      }
    }
    return changed;
  };

  const patchAvailabilityFields = (value) => {
    if (!value || typeof value !== "object") return false;
    let changed = false;
    for (const key of ["availableModels", "available_models"]) {
      const current = value[key];
      if (current instanceof Set) {
        for (const name of state.names) {
          if (!current.has(name)) {
            current.add(name);
            changed = true;
          }
        }
      } else if (Array.isArray(current) && current.every((item) => typeof item === "string")) {
        for (const name of state.names) {
          if (!current.includes(name)) {
            current.push(name);
            changed = true;
          }
        }
      }
    }
    for (const key of ["hiddenModels", "hidden_models"]) {
      if (!Array.isArray(value[key]) || !value[key].every((item) => typeof item === "string")) continue;
      const next = value[key].filter((name) => !state.names.includes(name));
      if (next.length !== value[key].length) {
        value[key] = next;
        changed = true;
      }
    }
    return changed;
  };

  const modelArrayCandidates = (result) => {
    const candidates = [];
    const seen = new Set();
    const add = (value, allowEmpty = false) => {
      if (!Array.isArray(value) || seen.has(value)) return;
      seen.add(value);
      candidates.push([value, allowEmpty]);
    };

    add(result, true);
    if (!result || typeof result !== "object") return candidates;
    add(result.models, true);
    add(result.data, true);
    add(result.result, true);
    add(result.result?.models, true);
    add(result.result?.data, true);
    add(result.message?.result?.models, true);
    add(result.message?.result?.data, true);
    add(result.pages?.[0]?.data, true);
    add(result.result?.pages?.[0]?.data, true);
    return candidates;
  };

  const explicitContainers = (result) => {
    if (!result || typeof result !== "object") return [];
    return [
      result,
      result.result,
      result.message,
      result.message?.result,
      result.data,
      result.result?.data,
    ].filter((value, index, list) => value && typeof value === "object" && list.indexOf(value) === index);
  };

  const patchModelListResult = (result) => {
    if (!result || typeof result !== "object" || !state.names.length) return false;
    let changed = false;
    let foundModelArray = false;
    for (const [models, allowEmpty] of modelArrayCandidates(result)) {
      if (!isModelDescriptorArray(models, allowEmpty)) continue;
      foundModelArray = true;
      if (patchModelDescriptorArray(models, allowEmpty)) changed = true;
    }
    for (const container of explicitContainers(result)) {
      if (patchAvailabilityFields(container)) changed = true;
    }
    if (foundModelArray) state.patchedModelResponses += 1;
    return changed;
  };

  const MODEL_CONFIG_NAME = "107580212";

  const patchStatsigModelConfig = (name, config) => {
    if (String(name) !== MODEL_CONFIG_NAME || !config?.value || typeof config.value !== "object") return config;
    const value = config.value;
    const available = Array.isArray(value.available_models)
      && value.available_models.every((item) => typeof item === "string")
      ? [...value.available_models]
      : [];
    for (const modelName of state.names) if (!available.includes(modelName)) available.push(modelName);
    const nextValue = {
      ...value,
      available_models: available,
      default_model: state.config.defaultModel || value.default_model || state.names[0],
    };
    try {
      config.value = nextValue;
      return config;
    } catch {
      return { ...config, value: nextValue };
    }
  };

  const statsigClients = () => {
    const root = globalThis.__STATSIG__;
    if (!root || typeof root !== "object") return [];
    const clients = [root.firstInstance];
    try { if (typeof root.instance === "function") clients.push(root.instance()); } catch {}
    if (root.instances && typeof root.instances === "object") clients.push(...Object.values(root.instances));
    return clients.filter((client, index, list) => client && typeof client === "object" && list.indexOf(client) === index);
  };

  const patchStatsig = () => {
    for (const client of statsigClients()) {
      if (state.patchedStatsigClients.has(client) || typeof client.getDynamicConfig !== "function") continue;
      const original = client.getDynamicConfig.bind(client);
      client.getDynamicConfig = (name, options) => patchStatsigModelConfig(name, original(name, options));
      state.patchedStatsigClients.add(client);
      state.patchedStatsigClientCount += 1;
      try { patchStatsigModelConfig(MODEL_CONFIG_NAME, client.getDynamicConfig(MODEL_CONFIG_NAME, { disableExposureLog: true })); } catch {}
    }
  };

  const patchMcpData = (data) => {
    if (!data || typeof data !== "object" || data.type !== "mcp-response") return false;
    const message = data.message || data.response;
    const id = message?.id == null ? "" : String(message.id);
    if (!id || !state.mcpRequestIds.has(id)) return false;
    state.mcpRequestIds.delete(id);
    return patchModelListResult(message?.result || message);
  };

  const installMessagePatch = () => {
    if (window.__ZNNZ_MESSAGE_PATCHED__) {
      state.messageHookInstalled = true;
      return;
    }
    window.__ZNNZ_MESSAGE_PATCHED__ = true;
    state.messageHookInstalled = true;
    const originalDispatch = window.dispatchEvent;
    window.dispatchEvent = function (event) {
      try {
        const detail = event?.detail;
        const request = detail?.request;
        if (event?.type === "codex-message-from-view" && detail?.type === "mcp-request" && request?.method === "model/list") {
          request.params = { ...(request.params || {}), includeHidden: true };
          if (request.id != null) state.mcpRequestIds.add(String(request.id));
        }
        if (event?.type === "message") patchMcpData(event.data);
      } catch (error) { logFailure("dispatchEvent", error); }
      return originalDispatch.call(this, event);
    };
    window.addEventListener("message", (event) => {
      try { patchMcpData(event.data); } catch (error) { logFailure("message", error); }
    }, true);
  };

  const actualRequestMethod = (method, params) => method === "send-cli-request-for-host"
    ? String(params?.method || "")
    : String(method || "");

  const includeHiddenParams = (method, params) => {
    const actual = actualRequestMethod(method, params);
    if (actual !== "model/list") return params;
    if (method === "send-cli-request-for-host") {
      return {
        ...(params || {}),
        params: { ...(params?.params || {}), includeHidden: true },
      };
    }
    return { ...(params || {}), includeHidden: true };
  };

  const patchRequestClient = (client) => {
    if (!client || typeof client.sendRequest !== "function" || state.patchedClients.has(client)) return false;
    const original = client.sendRequest.bind(client);
    client.sendRequest = async function (method, params, options) {
      const nextParams = includeHiddenParams(method, params);
      const result = await original(method, nextParams, options);
      const actual = actualRequestMethod(method, nextParams);
      if (actual === "list-models-for-host" || actual === "model/list") patchModelListResult(result);
      return result;
    };
    state.patchedClients.add(client);
    state.patchedClientCount += 1;
    return true;
  };

  const scanWebpackClients = () => {
    for (const key of Object.keys(globalThis)) {
      if (!key.startsWith("webpackChunk") || !Array.isArray(globalThis[key])) continue;
      try {
        let runtime;
        const chunk = globalThis[key];
        const marker = `znnz_${Date.now()}_${Math.random()}`;
        chunk.push([[marker], {}, (require) => { runtime = require; }]);
        chunk.pop();
        const cache = runtime?.c;
        if (!cache || typeof cache !== "object") continue;
        for (const module of Object.values(cache)) {
          const exports = module?.exports;
          if (!exports) continue;
          patchRequestClient(exports);
          if (typeof exports === "object") {
            for (const candidate of Object.values(exports).slice(0, 80)) {
              patchRequestClient(candidate);
              try { if (candidate && typeof candidate.get === "function") patchRequestClient(candidate.get()); } catch {}
            }
          }
        }
      } catch (error) { logFailure("webpack", error); }
    }
  };

  const refreshPass = () => {
    if (!state.names.length) return;
    try { patchStatsig(); } catch (error) { logFailure("statsig", error); }
    try { scanWebpackClients(); } catch (error) { logFailure("request-client", error); }
    try { patchModelPickerLabels(); } catch (error) { logFailure("model-labels", error); }
  };

  const configure = (config) => {
    state.config = { ...state.config, ...(config || {}) };
    loadCatalog(true).then(() => {
      for (let i = 0; i < 30; i += 1) setTimeout(refreshPass, i * 150);
    }).catch((error) => logFailure("catalog", error));
  };

  globalThis.__ZNNZ_MODEL_UNLOCK__ = {
    version: incoming.version || "unknown",
    configure,
    refresh: () => loadCatalog(true).then(refreshPass),
    health: () => ({
      installed: true,
      modelCount: state.names.length,
      loadedAt: state.loadedAt,
      catalogSource: state.catalogSource,
      messageHookInstalled: state.messageHookInstalled,
      pickerInteractionHookInstalled: state.pickerInteractionHookInstalled,
      patchedClientCount: state.patchedClientCount,
      patchedStatsigClientCount: state.patchedStatsigClientCount,
      patchedModelResponses: state.patchedModelResponses,
      patchedLabelCount: state.patchedLabelCount,
      failures: state.failures.slice(-5),
      href: location.href,
    }),
    _test: {
      patchModelListResult,
      patchMcpData,
      trackMcpRequestId: (id) => state.mcpRequestIds.add(String(id)),
      patchStatsigModelConfig,
      modelNameForLabel,
      patchModelPickerLabels,
      isModelPickerInteraction,
      schedulePickerLabelRefresh,
    },
  };

  installMessagePatch();
  installPickerInteractionPatch();
  configure(incoming);
  setInterval(refreshPass, 2000);
})();
