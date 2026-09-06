"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

async function main() {
  const listeners = new Map();
  const context = vm.createContext({
    console,
    location: { href: "app://-/index.html" },
    fetch: async () => { throw new Error("fetch must not be used when inline catalog exists"); },
    setTimeout: (callback) => { callback(); return 1; },
    clearTimeout: () => {},
    setInterval: () => 1,
  });
  context.window = context;
  context.addEventListener = (type, listener) => {
    const values = listeners.get(type) || [];
    values.push(listener);
    listeners.set(type, values);
  };
  context.dispatchEvent = () => true;
  context.__ZNNZ_CLIENT_CONFIG__ = {
    version: "test",
    defaultModel: "gpt-5.6-sol",
    modelDescription: "From https://api.znnz.net",
    catalog: {
      models: [
        {
          slug: "gpt-5.6-sol",
          visibility: "list",
          supported_in_api: true,
          default_reasoning_level: "medium",
          supported_reasoning_levels: [
            { effort: "low", description: "Low" },
            { effort: "medium", description: "Medium" },
          ],
        },
        { slug: "gpt-5.6-sol-max", visibility: "list", supported_in_api: true },
      ],
    },
  };

  const script = fs.readFileSync(path.join(__dirname, "..", "assets", "renderer-inject.js"), "utf8");
  vm.runInContext(script, context, { filename: "renderer-inject.js" });
  await Promise.resolve();
  await Promise.resolve();

  const api = context.__ZNNZ_MODEL_UNLOCK__;
  assert.equal(api.health().modelCount, 2);
  assert.equal(api.health().catalogSource, "inline");

  const recentTasks = ["task-one", "task-two"];
  const taskPayload = { recent: recentTasks, data: ["task-three"] };
  const taskSnapshot = JSON.stringify(taskPayload);
  assert.equal(api._test.patchModelListResult(taskPayload), false);
  assert.equal(JSON.stringify(taskPayload), taskSnapshot, "arbitrary task/string arrays must remain untouched");

  const descriptorPayload = { data: [{ id: "task-id", name: "Task title" }] };
  const descriptorSnapshot = JSON.stringify(descriptorPayload);
  assert.equal(api._test.patchModelListResult(descriptorPayload), false);
  assert.equal(JSON.stringify(descriptorPayload), descriptorSnapshot, "non-model descriptor arrays must remain untouched");

  const modelResult = {
    data: [{
      model: "gpt-5.6-sol",
      displayName: "5.6 Sol",
      description: "Official description",
      supportedReasoningEfforts: [{ reasoningEffort: "high", description: "High" }],
      requiredArray: ["kept"],
    }],
  };
  assert.equal(api._test.patchModelListResult(modelResult), true);
  assert.equal(modelResult.data.length, 2);
  assert.equal(modelResult.data[0].displayName, "gpt-5.6-sol");
  assert.equal(modelResult.data[0].description, "From https://api.znnz.net");
  assert.deepEqual(Array.from(modelResult.data[0].requiredArray), ["kept"]);
  assert.ok(Array.isArray(modelResult.data[0].supportedReasoningEfforts));
  assert.ok(modelResult.data[0].supportedReasoningEfforts.length > 0);
  assert.equal(modelResult.data[1].model, "gpt-5.6-sol-max");
  assert.equal(modelResult.data[1].displayName, "gpt-5.6-sol-max");
  assert.deepEqual(Array.from(modelResult.data[1].requiredArray), ["kept"], "new descriptors clone required template fields");

  const untracked = { type: "mcp-response", message: { id: 7, result: { data: [{ model: "gpt-5.6-sol" }] } } };
  const untrackedSnapshot = JSON.stringify(untracked);
  assert.equal(api._test.patchMcpData(untracked), false);
  assert.equal(JSON.stringify(untracked), untrackedSnapshot, "untracked MCP responses must not be patched");

  api._test.trackMcpRequestId(7);
  assert.equal(api._test.patchMcpData(untracked), true);
  assert.equal(untracked.message.result.data.length, 2);

  const unrelatedConfig = { value: { available_models: ["official"] } };
  const unrelatedSnapshot = JSON.stringify(unrelatedConfig);
  api._test.patchStatsigModelConfig("another-config", unrelatedConfig);
  assert.equal(JSON.stringify(unrelatedConfig), unrelatedSnapshot, "unrelated Statsig configs must remain untouched");

  const modelConfig = { value: { available_models: ["official"], default_model: "official" } };
  api._test.patchStatsigModelConfig("107580212", modelConfig);
  assert.deepEqual(Array.from(modelConfig.value.available_models), ["official", "gpt-5.6-sol", "gpt-5.6-sol-max"]);
  assert.equal(modelConfig.value.default_model, "gpt-5.6-sol");

  assert.equal(api._test.modelNameForLabel("5.6 Sol"), "gpt-5.6-sol");
  assert.equal(api._test.modelNameForLabel("5.6 Sol Max"), "gpt-5.6-sol-max");
  assert.equal(api._test.modelNameForLabel("unknown model"), null);

  const leaf = (text) => ({ textContent: text, children: [] });
  const container = (text, attributes = {}) => {
    const textLeaf = leaf(text);
    const attrs = { ...attributes };
    return {
      children: [textLeaf],
      textLeaf,
      querySelectorAll: (selector) => selector === "*" ? [textLeaf] : [],
      getAttribute: (name) => attrs[name] ?? null,
      setAttribute: (name, value) => { attrs[name] = value; },
      closest: () => null,
      attrs,
    };
  };

  const pickerTrigger = container("5.6 Sol");
  pickerTrigger.closest = (selector) => selector === 'button[data-codex-intelligence-trigger="true"]'
    ? pickerTrigger
    : null;
  const modelSubmenuTrigger = container("5.6 Sol", {
    "aria-label": "模型 5.6 Sol",
    "aria-controls": "gateway-model-menu",
  });
  const allModelsTrigger = container("All models", {
    "aria-controls": "gateway-model-menu",
  });
  allModelsTrigger.closest = (selector) => selector === '[role="menuitem"][aria-haspopup="menu"][aria-controls]'
    ? allModelsTrigger
    : null;
  const solRow = container("5.6 Sol");
  const solMaxRow = container("5.6 Sol Max");
  const reasoningRow = container("高");
  const modelMenu = {
    querySelectorAll: () => [solRow, solMaxRow, reasoningRow],
  };
  for (const row of [solRow, solMaxRow, reasoningRow]) row.closest = () => modelMenu;
  const ordinaryChatText = leaf("5.6 Sol");

  context.document = {
    querySelectorAll: (selector) => {
      if (selector === 'button[data-codex-intelligence-trigger="true"]') return [pickerTrigger];
      if (selector === '[role="menuitem"][aria-haspopup="menu"][aria-controls]') return [modelSubmenuTrigger, allModelsTrigger];
      return [];
    },
    getElementById: (id) => id === "gateway-model-menu" ? modelMenu : null,
  };

  assert.equal(api._test.patchModelPickerLabels(), 3);
  assert.equal(pickerTrigger.textLeaf.textContent, "5.6 Sol", "current model trigger must keep the native label");
  assert.equal(modelSubmenuTrigger.textLeaf.textContent, "gpt-5.6-sol");
  assert.equal(modelSubmenuTrigger.attrs["aria-label"], "模型 gpt-5.6-sol");
  assert.equal(solRow.textLeaf.textContent, "gpt-5.6-sol");
  assert.equal(solMaxRow.textLeaf.textContent, "gpt-5.6-sol-max");
  assert.equal(reasoningRow.textLeaf.textContent, "高");
  assert.equal(ordinaryChatText.textContent, "5.6 Sol", "ordinary chat/task text must remain untouched");
  assert.equal(api._test.patchModelPickerLabels(), 0, "label patch must be idempotent");

  assert.equal(api.health().pickerInteractionHookInstalled, true);
  const pointerListeners = listeners.get("pointerdown") || [];
  assert.equal(pointerListeners.length, 1, "picker pointer hook must be installed exactly once");

  pickerTrigger.textLeaf.textContent = "5.6 Sol";
  modelSubmenuTrigger.textLeaf.textContent = "5.6 Sol";
  modelSubmenuTrigger.attrs["aria-label"] = "Model 5.6 Sol";
  solRow.textLeaf.textContent = "5.6 Sol";
  solMaxRow.textLeaf.textContent = "5.6 Sol Max";
  pointerListeners[0]({ target: allModelsTrigger });
  assert.equal(pickerTrigger.textLeaf.textContent, "5.6 Sol", "current model trigger must keep the native label after refresh");
  assert.equal(modelSubmenuTrigger.textLeaf.textContent, "gpt-5.6-sol");
  assert.equal(solRow.textLeaf.textContent, "gpt-5.6-sol");
  assert.equal(solMaxRow.textLeaf.textContent, "gpt-5.6-sol-max");

  solRow.textLeaf.textContent = "5.6 Sol";
  pointerListeners[0]({ target: ordinaryChatText });
  assert.equal(solRow.textLeaf.textContent, "5.6 Sol", "ordinary clicks must not scan or patch the picker");
  assert.equal(ordinaryChatText.textContent, "5.6 Sol", "ordinary chat/task text must remain untouched");

  assert.equal(script.includes("Response.prototype.json"), false);
  assert.equal(script.includes("MutationObserver"), false);
  assert.equal(script.includes("patchReact"), false);
  assert.equal(script.includes("patchGraph"), false);
  assert.equal(script.includes("patchNameArray"), false);

  console.log("renderer injection regression tests passed");
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
