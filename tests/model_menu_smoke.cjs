const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../assets/renderer-inject.js'), 'utf8');
const catalog = (hidden) => ({
  models: ['gpt-old', 'gpt-new'].map((slug) => ({
    slug, supported_in_api: true, visibility: hidden.includes(slug) ? 'hide' : 'list',
  })),
  agent_switch_hidden_models: hidden,
});

async function main() {
  const context = vm.createContext({
    console, location: { href: 'https://synthetic.test' },
    setTimeout() {}, setInterval() {},
    window: { dispatchEvent() {}, addEventListener() {} },
    __ZNNZ_CLIENT_CONFIG__: { catalog: catalog(['gpt-old']), version: 'test' },
  });
  vm.runInContext(source, context);
  const unlock = context.__ZNNZ_MODEL_UNLOCK__;
  await unlock.refresh();
  const response = {
    models: [{ model: 'gpt-old' }, { model: 'native-other' }],
    availableModels: ['gpt-old', 'native-other'], hiddenModels: ['gpt-new'],
  };
  unlock._test.patchModelListResult(response);
  assert.deepEqual(response.models.map((model) => model.model), ['native-other', 'gpt-new']);
  assert.deepEqual(response.availableModels, ['native-other', 'gpt-new']);
  assert.equal(JSON.stringify(response.hiddenModels), '["gpt-old"]');
  const statsig = unlock._test.patchStatsigModelConfig('107580212', {
    value: { available_models: ['gpt-old', 'native-other'] },
  });
  assert.equal(JSON.stringify(statsig.value.available_models), '["native-other","gpt-new"]');

  const reply = { model: 'gpt-old', output: [{ type: 'message', content: 'still generating' }] };
  const before = JSON.stringify(reply);
  unlock._test.patchModelListResult(reply);
  assert.equal(JSON.stringify(reply), before);
  const unrelated = { type: 'mcp-response', message: { id: 42, result: { models: [{ model: 'gpt-old' }] } } };
  assert.equal(unlock._test.patchMcpData(unrelated), false);
  assert.equal(unrelated.message.result.models[0].model, 'gpt-old');

  unlock.configure({ catalog: catalog(['gpt-old', 'gpt-new']), allowEmptyModelList: true });
  await unlock.refresh();
  const empty = { models: [{ model: 'gpt-old' }, { model: 'gpt-new' }] };
  unlock._test.patchModelListResult(empty);
  assert.deepEqual(empty.models, []);
  assert.equal(unlock.health().modelCount, 0);
  assert.ok(unlock.health().loadedAt > 0);

  unlock.configure({ catalog: catalog([]) });
  await unlock.refresh();
  unlock._test.patchModelListResult(empty);
  assert.deepEqual(empty.models.map((model) => model.model), ['gpt-old', 'gpt-new']);
  console.log('Model menu smoke test passed: hide, restore, empty picker, and unrelated replies.');
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
