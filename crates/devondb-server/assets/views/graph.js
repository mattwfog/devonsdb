import { createForceSimulation } from "./force.js";
import { createEntityPanel } from "./entity.js";

const SVG_NAMESPACE = ["http:", "", "www.w3.org", "2000", "svg"].join("/");
const VIEW_WIDTH = 900;
const VIEW_HEIGHT = 460;
const NODE_LIMIT = 500;
const SETTLED_ENERGY = 0.025;
const NODE_SELECTION_DELAY = 220;
const TABLE_PALETTE = Object.freeze([
  "var(--accent)",
  "var(--warning)",
  "var(--danger)",
  "var(--text)",
  "var(--border-strong)",
]);

const controllers = new WeakMap();

export function registerGraphView(registerView) {
  registerView("Graph", { render: renderGraph });
}

function renderGraph(container, model) {
  let controller = controllers.get(container);
  if (!controller) {
    controller = createGraphController(container);
    controllers.set(container, controller);
  }
  controller.attach(model);
}

function createGraphController(container) {
  const dom = graphDom();
  const state = {
    dom,
    nodes: new Map(),
    edges: new Map(),
    simulation: null,
    simulationNodes: new Map(),
    nodeViews: new Map(),
    edgeViews: new Map(),
    frame: null,
    needsTick: false,
    busy: false,
    truncated: false,
    drag: null,
    schema: null,
    entity: null,
    entityGraph: null,
    entityRequest: 0,
    selectionTimer: null,
  };
  state.entity = createEntityPanel((node) => {
    void loadEntity(state, node, true);
  });
  dom.layout.append(dom.wrapper, state.entity.element);
  bindGraphEvents(state);

  return Object.freeze({
    attach(model) {
      const schema = model?.schema ?? null;
      if (schema !== state.schema) {
        state.schema = schema;
        renderCurrentEntity(state);
      }
      if (dom.root.parentNode !== container) {
        container.replaceChildren(dom.root);
      }
      if (state.needsTick) {
        requestTick(state);
      }
    },
  });
}

function graphDom() {
  const root = document.createElement("section");
  root.className = "graph-view";
  root.style.display = "grid";
  root.style.gap = "8px";

  const form = neighborhoodForm();
  const feedback = document.createElement("div");
  feedback.setAttribute("aria-live", "polite");
  const error = document.createElement("div");
  error.className = "result-error";
  error.setAttribute("role", "alert");
  error.hidden = true;
  feedback.append(error);

  const summary = document.createElement("div");
  summary.className = "result-summary";
  summary.textContent = "Enter a table and primary key to view a neighborhood.";
  const truncated = document.createElement("span");
  truncated.className = "warning-badge";
  truncated.textContent = "truncated neighborhood";
  truncated.hidden = true;
  summary.append(truncated);

  const canvas = graphCanvas();
  const layout = document.createElement("div");
  layout.className = "graph-layout";
  root.append(form.element, feedback, summary, layout);
  return {
    root,
    layout,
    form: form.element,
    tableInput: form.tableInput,
    keyInput: form.keyInput,
    submit: form.submit,
    error,
    summary,
    truncated,
    ...canvas,
  };
}

function neighborhoodForm() {
  const form = document.createElement("form");
  form.className = "canonical-block";
  form.setAttribute("aria-label", "View graph neighborhood");

  const title = document.createElement("span");
  title.className = "section-label";
  title.textContent = "view neighborhood";

  const fields = document.createElement("div");
  fields.style.display = "flex";
  fields.style.alignItems = "end";
  fields.style.flexWrap = "wrap";
  fields.style.gap = "7px";
  const table = graphInput("table", "Person");
  const key = graphInput("key", "1 or Ada");

  const submit = document.createElement("button");
  submit.className = "button primary";
  submit.type = "submit";
  submit.textContent = "View";
  fields.append(table.label, key.label, submit);
  form.append(title, fields);
  return {
    element: form,
    tableInput: table.input,
    keyInput: key.input,
    submit,
  };
}

function graphInput(name, placeholder) {
  const label = document.createElement("label");
  label.style.display = "grid";
  label.style.gap = "3px";
  label.style.color = "var(--muted)";
  label.style.fontSize = "0.72rem";
  label.textContent = name;

  const input = document.createElement("input");
  input.name = name;
  input.required = true;
  input.autocomplete = "off";
  input.placeholder = placeholder;
  input.style.minWidth = name === "table" ? "150px" : "180px";
  input.style.padding = "6px 7px";
  input.style.border = "1px solid var(--border-strong)";
  input.style.borderRadius = "4px";
  input.style.background = "var(--panel)";
  input.style.color = "var(--text)";
  input.style.font = "inherit";
  label.append(input);
  return { label, input };
}

function graphCanvas() {
  const wrapper = document.createElement("div");
  wrapper.className = "graph-canvas";
  wrapper.style.position = "relative";
  wrapper.style.overflow = "hidden";
  wrapper.style.border = "1px solid var(--border)";
  wrapper.style.borderRadius = "4px";
  wrapper.style.background = "var(--panel-muted)";

  const svg = svgElement("svg");
  svg.setAttribute("viewBox", `0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Force-directed graph neighborhood");
  svg.style.width = "100%";
  svg.style.height = "min(52vh, 460px)";
  svg.style.minHeight = "300px";
  svg.style.display = "block";
  svg.style.touchAction = "none";

  const background = svgElement("rect");
  background.setAttribute("width", String(VIEW_WIDTH));
  background.setAttribute("height", String(VIEW_HEIGHT));
  background.setAttribute("fill", "transparent");
  const edgeLayer = svgElement("g");
  const nodeLayer = svgElement("g");
  svg.append(background, edgeLayer, nodeLayer);

  const empty = ghostPlaceholder();

  const hoverCard = document.createElement("div");
  hoverCard.hidden = true;
  hoverCard.style.position = "absolute";
  hoverCard.style.zIndex = "2";
  hoverCard.style.maxWidth = "320px";
  hoverCard.style.padding = "8px 9px";
  hoverCard.style.border = "1px solid var(--border-strong)";
  hoverCard.style.borderRadius = "4px";
  hoverCard.style.background = "var(--panel)";
  hoverCard.style.color = "var(--text)";
  hoverCard.style.boxShadow = "var(--shadow)";
  hoverCard.style.font = "0.74rem/1.4 ui-monospace, monospace";
  hoverCard.style.pointerEvents = "none";
  wrapper.append(svg, empty, hoverCard);
  return { wrapper, svg, background, edgeLayer, nodeLayer, empty, hoverCard };
}

function ghostPlaceholder() {
  const ghost = document.createElement("div");
  ghost.className = "graph-ghost";

  const art = svgElement("svg");
  art.classList.add("graph-ghost-art");
  art.setAttribute("viewBox", "0 0 250 115");
  art.setAttribute("aria-hidden", "true");
  art.setAttribute("focusable", "false");
  const edges = [[47, 68, 103, 30], [47, 68, 112, 91], [103, 30, 175, 48], [112, 91, 175, 48], [175, 48, 219, 82]];
  for (const [x1, y1, x2, y2] of edges) {
    const edge = svgElement("line");
    edge.classList.add("graph-ghost-edge");
    edge.setAttribute("x1", String(x1));
    edge.setAttribute("y1", String(y1));
    edge.setAttribute("x2", String(x2));
    edge.setAttribute("y2", String(y2));
    art.append(edge);
  }
  const nodes = [[47, 68], [103, 30], [112, 91], [175, 48], [219, 82]];
  nodes.forEach(([cx, cy], index) => {
    const node = svgElement("circle");
    node.classList.add("graph-ghost-node");
    if (index % 2 === 1) {
      node.classList.add("secondary");
    }
    node.setAttribute("cx", String(cx));
    node.setAttribute("cy", String(cy));
    node.setAttribute("r", index === 3 ? "13" : "10");
    art.append(node);
  });

  const message = document.createElement("p");
  message.textContent = "Run a query or load a neighborhood to explore the graph.";
  ghost.append(art, message);
  return ghost;
}

function bindGraphEvents(state) {
  state.dom.form.addEventListener("submit", (event) => {
    event.preventDefault();
    const table = state.dom.tableInput.value.trim();
    const keyText = state.dom.keyInput.value.trim();
    if (table.length === 0 || keyText.length === 0) {
      showError(state, "Enter both a table and a primary key.");
      return;
    }
    void loadNeighborhood(state, table, parseKey(keyText), false);
  });
  state.dom.svg.addEventListener("pointermove", (event) => dragMove(state, event));
  state.dom.svg.addEventListener("pointerup", (event) => dragEnd(state, event));
  state.dom.svg.addEventListener("pointercancel", (event) => dragEnd(state, event));
  state.dom.svg.addEventListener("dblclick", (event) => releaseCanvas(state, event));
}

async function loadNeighborhood(state, table, key, merge) {
  if (state.busy) {
    showError(state, "Wait for the current neighborhood request to finish.");
    return;
  }
  clearError(state);
  cancelEntityLoad(state);
  setBusy(state, true);
  try {
    const response = await requestJson("/api/graph", {
      method: "POST",
      body: { table, key, limit: NODE_LIMIT },
    });
    const graph = normalizeGraphResponse(response, state.schema);
    if (merge) {
      mergeNeighborhood(state, graph);
    } else {
      clearEntity(state);
      replaceNeighborhood(state, graph);
    }
  } catch (error) {
    showError(state, errorMessage(error));
  } finally {
    setBusy(state, false);
  }
}

async function loadEntity(state, node, recenterCanvas) {
  if (state.busy) {
    state.entity.showError("Wait for the current neighborhood request to finish.");
    return;
  }
  const request = state.entityRequest + 1;
  state.entityRequest = request;
  clearError(state);
  state.entity.showLoading(node);
  try {
    const response = await requestJson("/api/graph", {
      method: "POST",
      body: { table: node.table, key: node.key, limit: NODE_LIMIT },
    });
    const graph = normalizeGraphResponse(response, state.schema);
    if (request !== state.entityRequest) {
      return;
    }
    const center = graph.nodes.find((item) => item.id === nodeIdentity(node.table, node.key));
    if (!center) {
      throw new Error("The server returned a neighborhood without its center entity.");
    }
    if (recenterCanvas) {
      state.dom.tableInput.value = center.table;
      state.dom.keyInput.value = inputKey(center.key);
      replaceNeighborhood(state, graph);
    }
    showEntity(state, center, graph);
  } catch (error) {
    if (request === state.entityRequest) {
      state.entityGraph = null;
      state.entity.showError(errorMessage(error));
    }
  }
}

function showEntity(state, node, graph) {
  state.entityGraph = {
    node,
    nodes: graph.nodes,
    edges: graph.edges,
    truncated: graph.truncated,
  };
  renderCurrentEntity(state);
}

function renderCurrentEntity(state) {
  if (state.entityGraph) {
    state.entity.render({ ...state.entityGraph, schema: state.schema });
  }
}

function clearEntity(state) {
  clearPendingSelection(state);
  state.entityRequest += 1;
  state.entityGraph = null;
  state.entity.showEmpty();
}

function cancelEntityLoad(state) {
  clearPendingSelection(state);
  state.entityRequest += 1;
  if (state.entityGraph) {
    renderCurrentEntity(state);
  } else {
    state.entity.showEmpty();
  }
}

function replaceNeighborhood(state, graph) {
  state.nodes = new Map();
  let omitted = false;
  for (const node of graph.nodes) {
    if (state.nodes.size === NODE_LIMIT) {
      omitted = true;
      break;
    }
    state.nodes.set(node.id, node);
  }
  state.edges = retainedEdges(graph.edges, state.nodes);
  state.truncated = graph.truncated || omitted;
  rebuildSimulation(state, false);
}

function mergeNeighborhood(state, graph) {
  let omitted = false;
  for (const node of graph.nodes) {
    if (state.nodes.has(node.id)) {
      state.nodes.set(node.id, node);
    } else if (state.nodes.size < NODE_LIMIT) {
      state.nodes.set(node.id, node);
    } else {
      omitted = true;
    }
  }
  for (const [id, edge] of retainedEdges(graph.edges, state.nodes)) {
    state.edges.set(id, edge);
  }
  state.truncated = state.truncated || graph.truncated || omitted;
  rebuildSimulation(state, true);
}

function retainedEdges(edges, nodes) {
  const retained = new Map();
  for (const edge of edges) {
    if (nodes.has(edge.source) && nodes.has(edge.target)) {
      retained.set(edge.id, edge);
    }
  }
  return retained;
}

function rebuildSimulation(state, preservePositions) {
  const previous = preservePositions ? simulationSnapshot(state) : new Map();
  const inputNodes = Array.from(state.nodes.values(), (node) => {
    const position = previous.get(node.id);
    return position ? { id: node.id, ...position } : { id: node.id };
  });
  const inputEdges = Array.from(state.edges.values(), (edge) => ({
    source: edge.source,
    target: edge.target,
  }));
  state.simulation = createForceSimulation(inputNodes, inputEdges);
  state.simulationNodes = new Map(
    state.simulation.nodes.map((node) => [node.id, node]),
  );
  rebuildSvg(state);
  resumeSimulation(state);
}

function simulationSnapshot(state) {
  if (!state.simulation) {
    return new Map();
  }
  return new Map(state.simulation.nodes.map((node) => [node.id, {
    x: node.x,
    y: node.y,
    vx: node.vx,
    vy: node.vy,
    pinned: node.pinned,
    fx: node.fx,
    fy: node.fy,
  }]));
}

function rebuildSvg(state) {
  state.dom.edgeLayer.replaceChildren();
  state.dom.nodeLayer.replaceChildren();
  state.edgeViews = new Map();
  state.nodeViews = new Map();

  for (const edge of state.edges.values()) {
    const line = edgeLine(edge);
    state.edgeViews.set(edge.id, line);
    state.dom.edgeLayer.append(line);
  }
  for (const node of state.nodes.values()) {
    const group = nodeGroup(state, node);
    state.nodeViews.set(node.id, group);
    state.dom.nodeLayer.append(group);
  }
  state.dom.empty.hidden = state.nodes.size > 0;
  updateSummary(state);
  renderPositions(state);
}

function edgeLine(edge) {
  const line = svgElement("line");
  line.setAttribute("stroke", "var(--border-strong)");
  line.setAttribute("stroke-width", "1.5");
  line.setAttribute("stroke-opacity", "0.8");
  const title = svgElement("title");
  title.textContent = edge.rel;
  line.append(title);
  return line;
}

function nodeGroup(state, node) {
  const group = svgElement("g");
  group.setAttribute("role", "button");
  group.setAttribute("tabindex", "0");
  group.setAttribute("aria-label", `${nodeHeading(node)}; select for details; double-click to expand`);
  group.style.cursor = "grab";

  const circle = svgElement("circle");
  circle.setAttribute("r", "11");
  circle.setAttribute("fill", tableColor(node.table));
  circle.setAttribute("stroke", "var(--panel)");
  circle.setAttribute("stroke-width", "2");

  const label = svgElement("text");
  label.setAttribute("x", "15");
  label.setAttribute("y", "4");
  label.setAttribute("fill", "var(--text)");
  label.setAttribute("stroke", "var(--panel)");
  label.setAttribute("stroke-width", "3");
  label.setAttribute("paint-order", "stroke");
  label.setAttribute("font-size", "11");
  label.setAttribute("font-family", "ui-monospace, monospace");
  label.textContent = node.label;
  group.append(circle, label);

  group.addEventListener("pointerdown", (event) => dragStart(state, node, event));
  group.addEventListener("pointerenter", (event) => showHoverCard(state, node, event));
  group.addEventListener("pointermove", (event) => positionHoverCard(state, event));
  group.addEventListener("pointerleave", () => hideHoverCard(state));
  group.addEventListener("keydown", (event) => selectFromKeyboard(state, node, event));
  group.addEventListener("dblclick", (event) => {
    event.preventDefault();
    event.stopPropagation();
    clearPendingSelection(state);
    expandNode(state, node);
  });
  return group;
}

function scheduleSelection(state, node) {
  clearPendingSelection(state);
  state.selectionTimer = window.setTimeout(() => {
    state.selectionTimer = null;
    void loadEntity(state, node, false);
  }, NODE_SELECTION_DELAY);
}

function selectFromKeyboard(state, node, event) {
  if (event.key !== "Enter" && event.key !== " ") {
    return;
  }
  event.preventDefault();
  clearPendingSelection(state);
  void loadEntity(state, node, false);
}

function clearPendingSelection(state) {
  if (state.selectionTimer !== null) {
    window.clearTimeout(state.selectionTimer);
    state.selectionTimer = null;
  }
}

function expandNode(state, node) {
  state.dom.tableInput.value = node.table;
  state.dom.keyInput.value = inputKey(node.key);
  void loadNeighborhood(state, node.table, node.key, true);
}

function dragStart(state, node, event) {
  if (event.button !== 0 || !state.simulation) {
    return;
  }
  clearPendingSelection(state);
  const simulationNode = state.simulationNodes.get(node.id);
  if (!simulationNode) {
    return;
  }
  const point = simulationPoint(state.dom.svg, event);
  state.drag = {
    id: node.id,
    pointerId: event.pointerId,
    startClientX: event.clientX,
    startClientY: event.clientY,
    offsetX: simulationNode.x - point.x,
    offsetY: simulationNode.y - point.y,
    wasPinned: simulationNode.pinned,
    originalX: simulationNode.x,
    originalY: simulationNode.y,
    originalFx: simulationNode.fx,
    originalFy: simulationNode.fy,
    moved: false,
  };
  state.simulation.pin(node.id, simulationNode.x, simulationNode.y);
  state.dom.svg.setPointerCapture(event.pointerId);
  hideHoverCard(state);
  event.preventDefault();
}

function dragMove(state, event) {
  const drag = state.drag;
  if (!drag || drag.pointerId !== event.pointerId || !state.simulation) {
    return;
  }
  const movement = Math.hypot(
    event.clientX - drag.startClientX,
    event.clientY - drag.startClientY,
  );
  drag.moved ||= movement > 3;
  const point = simulationPoint(state.dom.svg, event);
  state.simulation.pin(
    drag.id,
    clamp(point.x + drag.offsetX, -VIEW_WIDTH / 2 + 20, VIEW_WIDTH / 2 - 20),
    clamp(point.y + drag.offsetY, -VIEW_HEIGHT / 2 + 20, VIEW_HEIGHT / 2 - 20),
  );
  renderPositions(state);
  resumeSimulation(state);
}

function dragEnd(state, event) {
  const drag = state.drag;
  if (!drag || drag.pointerId !== event.pointerId || !state.simulation) {
    return;
  }
  const selectedNode = event.type === "pointerup" && !drag.moved
    ? state.nodes.get(drag.id)
    : null;
  if (!drag.moved) {
    if (drag.wasPinned) {
      state.simulation.pin(drag.id, drag.originalFx, drag.originalFy);
    } else {
      const node = state.simulation.unpin(drag.id);
      if (node) {
        node.x = drag.originalX;
        node.y = drag.originalY;
      }
    }
  }
  state.drag = null;
  if (state.dom.svg.hasPointerCapture(event.pointerId)) {
    state.dom.svg.releasePointerCapture(event.pointerId);
  }
  if (selectedNode) {
    scheduleSelection(state, selectedNode);
  }
  resumeSimulation(state);
}

function releaseCanvas(state, event) {
  if (event.target !== state.dom.svg && event.target !== state.dom.background) {
    return;
  }
  state.simulation?.unpinAll();
  resumeSimulation(state);
}

function simulationPoint(svg, event) {
  const bounds = svg.getBoundingClientRect();
  return {
    x: (event.clientX - bounds.left) * VIEW_WIDTH / bounds.width - VIEW_WIDTH / 2,
    y: (event.clientY - bounds.top) * VIEW_HEIGHT / bounds.height - VIEW_HEIGHT / 2,
  };
}

function resumeSimulation(state) {
  state.needsTick = true;
  requestTick(state);
}

function requestTick(state) {
  if (state.frame === null && state.dom.root.isConnected && state.simulation) {
    state.frame = window.requestAnimationFrame(() => animationTick(state));
  }
}

function animationTick(state) {
  state.frame = null;
  if (!state.dom.root.isConnected || !state.simulation) {
    return;
  }
  try {
    const energy = state.simulation.tick();
    renderPositions(state);
    state.needsTick = energy >= SETTLED_ENERGY;
    if (state.needsTick) {
      requestTick(state);
    }
  } catch (error) {
    state.needsTick = false;
    showError(state, `Graph simulation failed: ${errorMessage(error)}`);
  }
}

function renderPositions(state) {
  for (const edge of state.edges.values()) {
    const line = state.edgeViews.get(edge.id);
    const source = state.simulationNodes.get(edge.source);
    const target = state.simulationNodes.get(edge.target);
    if (!line || !source || !target) {
      continue;
    }
    line.setAttribute("x1", String(VIEW_WIDTH / 2 + source.x));
    line.setAttribute("y1", String(VIEW_HEIGHT / 2 + source.y));
    line.setAttribute("x2", String(VIEW_WIDTH / 2 + target.x));
    line.setAttribute("y2", String(VIEW_HEIGHT / 2 + target.y));
  }
  for (const node of state.simulation?.nodes ?? []) {
    const group = state.nodeViews.get(node.id);
    if (group) {
      group.setAttribute(
        "transform",
        `translate(${VIEW_WIDTH / 2 + node.x} ${VIEW_HEIGHT / 2 + node.y})`,
      );
    }
  }
}

function updateSummary(state) {
  state.dom.summary.replaceChildren();
  const text = document.createElement("span");
  const nodeCount = state.nodes.size;
  const edgeCount = state.edges.size;
  text.textContent = nodeCount === 0
    ? "Enter a table and primary key to view a neighborhood."
    : `${nodeCount} nodes · ${edgeCount} edges · click for details · drag to pin · double-click to expand`;
  state.dom.summary.append(text);
  state.dom.truncated.hidden = !state.truncated;
  state.dom.summary.append(state.dom.truncated);
}

function showHoverCard(state, node, event) {
  const card = state.dom.hoverCard;
  card.replaceChildren();
  const heading = document.createElement("strong");
  heading.textContent = nodeHeading(node);
  card.append(heading);

  const entries = Object.entries(node.props);
  if (entries.length === 0) {
    const empty = document.createElement("div");
    empty.style.color = "var(--muted)";
    empty.textContent = "no properties";
    card.append(empty);
  } else {
    for (const [name, value] of entries) {
      const row = document.createElement("div");
      row.style.overflowWrap = "anywhere";
      row.textContent = `${name}: ${displayValue(value)}`;
      card.append(row);
    }
  }
  card.hidden = false;
  positionHoverCard(state, event);
}

function positionHoverCard(state, event) {
  if (state.dom.hoverCard.hidden) {
    return;
  }
  const bounds = state.dom.wrapper.getBoundingClientRect();
  const card = state.dom.hoverCard;
  const proposedLeft = event.clientX - bounds.left + 12;
  const proposedTop = event.clientY - bounds.top + 12;
  const maximumLeft = Math.max(8, bounds.width - card.offsetWidth - 8);
  const maximumTop = Math.max(8, bounds.height - card.offsetHeight - 8);
  card.style.left = `${clamp(proposedLeft, 8, maximumLeft)}px`;
  card.style.top = `${clamp(proposedTop, 8, maximumTop)}px`;
}

function hideHoverCard(state) {
  state.dom.hoverCard.hidden = true;
}

function normalizeGraphResponse(value, schema) {
  if (!isObject(value) || !Array.isArray(value.nodes) || !Array.isArray(value.edges)
      || typeof value.truncated !== "boolean") {
    throw new Error("The server returned a malformed graph response.");
  }
  const nodes = normalizeNodes(value.nodes, schema);
  const resolveEndpoint = endpointResolver(nodes);
  const edges = normalizeEdges(value.edges, resolveEndpoint);
  return { nodes, edges, truncated: value.truncated };
}

function normalizeNodes(values, schema) {
  const nodes = new Map();
  for (const value of values) {
    if (!isObject(value) || typeof value.table !== "string"
        || !Object.hasOwn(value, "key") || !isObject(value.props)) {
      throw new Error("The server returned a malformed graph node.");
    }
    const id = nodeIdentity(value.table, value.key);
    const identityLabel = `${value.table}:${displayValue(value.key)}`;
    nodes.set(id, {
      id,
      table: value.table,
      key: value.key,
      props: value.props,
      identityLabel,
      label: nodeDisplayLabel(value.table, value.props, identityLabel, schema),
    });
  }
  return Array.from(nodes.values());
}

function normalizeEdges(values, resolveEndpoint) {
  const edges = new Map();
  for (const value of values) {
    if (!isObject(value) || typeof value.rel !== "string"
        || !Object.hasOwn(value, "from") || !Object.hasOwn(value, "to")) {
      throw new Error("The server returned a malformed graph edge.");
    }
    const source = resolveEndpoint(value.from);
    const target = resolveEndpoint(value.to);
    if (source === null || target === null) {
      throw new Error("The server returned a graph edge with an unknown endpoint.");
    }
    const id = JSON.stringify([value.rel, source, target]);
    edges.set(id, { id, rel: value.rel, source, target });
  }
  return Array.from(edges.values());
}

function endpointResolver(nodes) {
  const nodeIds = new Set(nodes.map((node) => node.id));
  const aliases = new Map();
  const ambiguous = new Set();
  for (const node of nodes) {
    addAlias(aliases, ambiguous, node.id, node.id);
    addAlias(aliases, ambiguous, node.identityLabel, node.id);
    addAlias(aliases, ambiguous, `${node.table}:${JSON.stringify(node.key)}`, node.id);
    addAlias(aliases, ambiguous, endpointToken(node.key), node.id);
  }
  return (endpoint) => {
    const direct = endpointIdentity(endpoint);
    if (direct !== null) {
      return nodeIds.has(direct) ? direct : null;
    }
    const candidates = typeof endpoint === "string"
      ? [endpoint, endpointToken(endpoint)]
      : [endpointToken(endpoint)];
    for (const candidate of candidates) {
      if (!ambiguous.has(candidate) && aliases.has(candidate)) {
        return aliases.get(candidate);
      }
    }
    return null;
  };
}

function endpointIdentity(endpoint) {
  if (isObject(endpoint) && typeof endpoint.table === "string"
      && Object.hasOwn(endpoint, "key")) {
    return nodeIdentity(endpoint.table, endpoint.key);
  }
  if (Array.isArray(endpoint) && endpoint.length === 2 && typeof endpoint[0] === "string") {
    return nodeIdentity(endpoint[0], endpoint[1]);
  }
  return null;
}

function addAlias(aliases, ambiguous, alias, id) {
  if (ambiguous.has(alias)) {
    return;
  }
  if (aliases.has(alias) && aliases.get(alias) !== id) {
    aliases.delete(alias);
    ambiguous.add(alias);
  } else {
    aliases.set(alias, id);
  }
}

function nodeIdentity(table, key) {
  return JSON.stringify([table, key]);
}

function endpointToken(value) {
  return `${typeof value}:${JSON.stringify(value)}`;
}

function nodeDisplayLabel(table, props, fallback, schema) {
  const classSummary = nodeClass(schema, table);
  if (typeof classSummary?.label === "string") {
    const entry = Object.entries(props).find(([name]) => sameFold(name, classSummary.label));
    return entry && entry[1] !== null ? displayValue(entry[1]) : fallback;
  }
  for (const preferredName of ["name", "title", "label"]) {
    for (const [name, value] of Object.entries(props)) {
      if (asciiFold(name) === preferredName && typeof value === "string") {
        return value;
      }
    }
  }
  return fallback;
}

function nodeClass(schema, table) {
  const classes = Array.isArray(schema?.classes?.node_classes)
    ? schema.classes.node_classes
    : [];
  return classes.find((item) => sameFold(item?.table, table)) ?? null;
}

function sameFold(left, right) {
  return typeof left === "string"
    && typeof right === "string"
    && asciiFold(left) === asciiFold(right);
}

function nodeHeading(node) {
  return node.label === node.identityLabel
    ? node.identityLabel
    : `${node.label} · ${node.identityLabel}`;
}

function asciiFold(value) {
  return value.replace(/[A-Z]/g, (character) => (
    String.fromCharCode(character.charCodeAt(0) + 32)
  ));
}

function tableColor(table) {
  let hash = 2_166_136_261;
  for (let index = 0; index < table.length; index += 1) {
    hash ^= table.charCodeAt(index);
    hash = Math.imul(hash, 16_777_619);
  }
  return TABLE_PALETTE[(hash >>> 0) % TABLE_PALETTE.length];
}

function parseKey(text) {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

function inputKey(value) {
  return typeof value === "string" ? value : JSON.stringify(value);
}

function displayValue(value) {
  if (typeof value === "string") {
    return value;
  }
  if (value === undefined) {
    return "undefined";
  }
  return JSON.stringify(value);
}

function setBusy(state, busy) {
  state.busy = busy;
  state.dom.tableInput.disabled = busy;
  state.dom.keyInput.disabled = busy;
  state.dom.submit.disabled = busy;
  state.dom.submit.textContent = busy ? "Loading…" : "View";
}

function showError(state, message) {
  state.dom.error.textContent = message;
  state.dom.error.hidden = false;
}

function clearError(state) {
  state.dom.error.replaceChildren();
  state.dom.error.hidden = true;
}

async function requestJson(path, options = {}) {
  if (!path.startsWith("/api/")) {
    throw new Error(`Refusing non-API request path: ${path}`);
  }
  const headers = { Accept: "application/json" };
  const request = { method: options.method ?? "GET", headers };
  if (options.body !== undefined) {
    headers["Content-Type"] = "application/json";
    request.body = JSON.stringify(options.body);
  }

  let response;
  try {
    response = await fetch(path, request);
  } catch (error) {
    throw new Error(`Request to ${path} failed: ${errorMessage(error)}`);
  }
  const payload = await responsePayload(response, path);
  if (!response.ok) {
    const detail = typeof payload?.error === "string"
      ? payload.error
      : `${response.status} ${response.statusText}`;
    throw new Error(detail);
  }
  return payload;
}

async function responsePayload(response, path) {
  const body = await response.text();
  if (body.length === 0) {
    return null;
  }
  try {
    return JSON.parse(body);
  } catch (error) {
    throw new Error(`Invalid JSON from ${path}: ${errorMessage(error)}`);
  }
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function svgElement(name) {
  return document.createElementNS(SVG_NAMESPACE, name);
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function clamp(value, minimum, maximum) {
  return Math.max(minimum, Math.min(maximum, value));
}
