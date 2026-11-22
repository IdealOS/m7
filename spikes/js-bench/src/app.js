// A stand-in for an idealos app: SolidJS-style signals driving scene-graph ops, roughly
// the shape @idealos/sdk will take in M4. `__send` is provided by the host and stands
// in for writing a frame to the bus socket.

let current = null;

function signal(value) {
  const subscribers = new Set();
  const read = () => {
    if (current) subscribers.add(current);
    return value;
  };
  const write = (next) => {
    if (next === value) return;
    value = next;
    for (const run of [...subscribers]) run();
  };
  return [read, write];
}

function effect(fn) {
  const run = () => {
    const previous = current;
    current = run;
    try {
      fn();
    } finally {
      current = previous;
    }
  };
  run();
}

let nextNode = 2; // node 1 is the window root
let nextRequest = 1;
const ops = [];

function node(type, props) {
  const id = nextNode++;
  ops.push({ op: "create", id, type, props });
  return id;
}

function insert(parent, child, index) {
  ops.push({ op: "insert", parent, child, index });
}

// One commit per batch, as the SDK will do once per microtask.
function flush() {
  if (ops.length === 0) return 0;
  const frame = JSON.stringify({
    id: nextRequest++,
    kind: "request",
    to: "compositor",
    method: "commit",
    params: { window: 1, ops },
  });
  ops.length = 0;
  __send(frame);
  return frame.length;
}

function buildApp(rows) {
  const root = node("vstack", { gap: 8, padding: 12 });
  insert(1, root, 0);
  const title = node("label", { text: "", style: "title" });
  insert(root, title, 0);
  const [count, setCount] = signal(0);
  effect(() => ops.push({ op: "set", id: title, props: { text: `Rows: ${count()}` } }));

  const setters = [];
  for (let i = 0; i < rows; i++) {
    const [text, setText] = signal(`row ${i}`);
    const id = node("label", { text: text() });
    insert(root, id, i + 1);
    effect(() => ops.push({ op: "set", id, props: { text: text() } }));
    setters.push(setText);
  }
  setCount(rows);
  flush();
  return setters;
}

globalThis.rows = buildApp(globalThis.ROWS ?? 500);

// Change one row per update, flushing every 100 updates.
globalThis.bench = (updates) => {
  for (let i = 0; i < updates; i++) {
    rows[i % rows.length](`row ${i} changed`);
    if (i % 100 === 99) flush();
  }
  flush();
};
