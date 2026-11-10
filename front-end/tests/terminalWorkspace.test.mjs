import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
import * as Vue from 'vue'
import { compileScript, parse } from 'vue/compiler-sfc'

// Mount the real workspace template with a lightweight renderer. Session stubs
// track the lifecycle that owns each WebSocket without needing a browser/PTY.
const rawSource = await readFile(new URL('../src/components/TerminalModal.vue', import.meta.url), 'utf8')
const { descriptor } = parse(rawSource)
const compiled = ts.transpileModule(compileScript(descriptor, {
  id: 'terminal-workspace-test',
  inlineTemplate: true,
}).content, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText
const source = compiled
  .replace(/import \{([^}]+)\} from ['"]vue['"];?/g, (_, names) =>
    `const { ${names.replace(/\bas\b/g, ':')} } = Vue`,
  )
  .replace(/import (\w+) from ['"][^'"]+\.vue['"];?/g, (_, name) => `const ${name} = dependencies.${name}`)
  .replace(/import \{[^}]+\} from ['"]@lucide\/vue['"];?/, 'const { LoaderCircle, Plus, RefreshCw, Server, TerminalSquare, X } = dependencies.icons')
  .replace(/import \{[^}]+\} from ['"]\.\.\/api\/terminal['"];?/, 'const { activeTerminalSessionCount, listTerminalShells, validCustomShellProgram } = dependencies.api')
  .replace('export default', 'return')
const loadWorkspace = new Function('Vue', 'dependencies', source)

function node(type, text = '') {
  return { type, text, props: {}, style: {}, children: [], parent: null }
}

function remove(child) {
  if (!child.parent) return
  const siblings = child.parent.children
  siblings.splice(siblings.indexOf(child), 1)
  child.parent = null
}

const { createApp } = Vue.createRenderer({
  createElement: (type) => node(type),
  createText: (text) => node('text', text),
  createComment: (text) => node('comment', text),
  setText: (target, text) => { target.text = text },
  setElementText: (target, text) => { target.text = text; target.children = [] },
  parentNode: (target) => target.parent,
  nextSibling: (target) => target.parent?.children[target.parent.children.indexOf(target) + 1] || null,
  patchProp: (target, key, _previous, value) => { target.props[key] = value },
  insert(child, parent, anchor = null) {
    remove(child)
    child.parent = parent
    parent.children.splice(anchor ? parent.children.indexOf(anchor) : parent.children.length, 0, child)
  },
  remove,
})

function find(root, predicate) {
  if (predicate(root)) return root
  for (const child of root.children) {
    const found = find(child, predicate)
    if (found) return found
  }
}

function textContent(root) {
  return root.text + root.children.map(textContent).join('')
}

test('new-session panel and tab switches preserve shells; explicit close disposes only its session', async () => {
  const mounted = []
  const disposed = []
  const active = new Map()
  const TerminalSession = Vue.defineComponent({
    props: ['instanceId', 'shellProgram', 'active'],
    emits: ['status'],
    setup(props, { emit }) {
      const id = mounted.length + 1
      mounted.push(id)
      Vue.watchEffect(() => active.set(id, props.active))
      Vue.onMounted(() => emit('status', 'ready'))
      Vue.onBeforeUnmount(() => { disposed.push(id); active.delete(id) })
      return () => Vue.h('div', { 'data-session-id': id })
    },
  })
  const Workspace = loadWorkspace(Vue, {
    UiButton: Vue.defineComponent({ setup: (_, { slots }) => () => Vue.h('button', slots.default?.()) }),
    UiInput: 'input', UiSelect: 'select', ConfirmModal: 'dialog', TerminalSession,
    icons: Object.fromEntries(['LoaderCircle', 'Plus', 'RefreshCw', 'Server', 'TerminalSquare', 'X'].map((name) => [name, 'span'])),
    api: {
      activeTerminalSessionCount: (sessions, instanceId) => sessions.filter((session) => session.instanceId === instanceId && ['opening', 'ready'].includes(session.status)).length,
      listTerminalShells: async () => ({ shells: [], max_sessions: 8 }),
      validCustomShellProgram: () => false,
    },
  })
  const instance = { id: 'node-1', hostname: 'test-host', online: true, os: 'linux', capabilities: [] }
  const app = createApp(Workspace, { instance, instances: [instance] })
  app.directive('tooltip', {})
  const root = node('root')
  app.mount(root)
  async function click(predicate) {
    const target = find(root, predicate)
    assert.ok(target, 'expected workspace control')
    target.props.onClick({ stopPropagation() {}, preventDefault() {} })
    await Vue.nextTick()
  }
  const newPanel = () => click((element) => element.props['aria-label'] === '新建终端')
  const create = () => click((element) => element.type === 'button' && textContent(element).trim() === '创建终端')
  try {
    await create()
    assert.deepEqual(mounted, [1])
    await newPanel()
    assert.deepEqual(disposed, [])
    assert.equal(active.get(1), false)
    await click((element) => element.type === 'button' && textContent(element).trim() === '取消')
    assert.equal(active.get(1), true)
    assert.deepEqual(mounted, [1])

    await newPanel()
    await create()
    assert.deepEqual(mounted, [1, 2])
    assert.deepEqual(disposed, [])
    assert.equal(active.get(1), false)
    assert.equal(active.get(2), true)
    await click((element) => element.props.role === 'tab' && textContent(element).includes('test-host') && !textContent(element).includes('#2'))
    assert.equal(active.get(1), true)
    assert.equal(active.get(2), false)
    assert.deepEqual(disposed, [])

    await click((element) => element.props['aria-label']?.startsWith('关闭 test-host') && !element.props['aria-label'].includes('#2'))
    assert.deepEqual(disposed, [1])
    assert.equal(active.get(2), true)
  } finally {
    app.unmount()
  }
  assert.deepEqual(disposed, [1, 2])
})
