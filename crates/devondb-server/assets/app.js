import { registerPlanView } from "./views/plan.js";
import { registerTableView } from "./views/table.js";
import { registerSchemaView } from "./views/schema.js";
import { registerGraphView } from "./views/graph.js";
import { createGridRequest, registerGridView } from "./views/grid.js";
import { createMapRequest, mapViewContext, registerMapView } from "./views/map.js";

const HISTORY_KEY = "devondb.sessionHistory.v1";
const STAGE_KEYWORDS = Object.freeze([
  "filter",
  "project",
  "expand",
  "sort",
  "limit",
  "aggregate",
]);
const RESERVED_WORDS = new Set([
  "aggregate", "and", "as", "asc", "avg", "both", "by", "cosine", "count",
  "create", "desc", "distance", "expand", "false", "filter", "from", "in",
  "insert", "into", "key", "knn", "l2", "limit", "max", "min", "node",
  "nodes", "not", "null", "offset", "or", "out", "primary", "project", "rel",
  "sort", "sum", "table", "to", "true", "values",
]);
const CARET_NAVIGATION_KEYS = new Set([
  "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown",
]);
const views = new Map();

export function registerView(name, view) {
  if (typeof name !== "string" || typeof view?.render !== "function") {
    throw new TypeError("A view requires a name and render function.");
  }
  if (views.has(name)) {
    throw new Error(`View ${name} is already registered.`);
  }
  views.set(name, Object.freeze({ ...view }));
}

registerPlanView(registerView);
registerTableView(registerView);
registerSchemaView(registerView);
registerGraphView(registerView);
registerGridView(registerView);
registerMapView(registerView);

const storedHistory = readHistory();
const store = createStore({
  editorText: "",
  explanation: null,
  explanationText: null,
  result: null,
  resultCanonical: null,
  schema: null,
  history: storedHistory.entries,
  editorError: null,
  resultErrors: storedHistory.error ? [storedHistory.error] : [],
  activeResultView: "Table",
  busyAction: null,
  schemaLoading: true,
  treeBusyNode: null,
  treeError: null,
  treeEditsActive: false,
  compiledFromNaturalLanguage: false,
  pinNameOpen: false,
  pinName: "",
  pinError: null,
  gridTable: null,
  gridSort: null,
  gridFilters: [],
  gridOffset: 0,
  gridResultTable: null,
  mapTable: null,
  mapColumn: null,
  mapResult: null,
  mapResultTable: null,
  mapResultColumn: null,
  mapBusy: false,
  mapError: null,
});

let dom;
let autocomplete = emptyAutocomplete();
let mapRequestVersion = 0;

function createStore(initialState) {
  let state = Object.freeze({ ...initialState });
  const subscribers = new Set();

  return Object.freeze({
    getState: () => state,
    subscribe(subscriber) {
      subscribers.add(subscriber);
      return () => subscribers.delete(subscriber);
    },
    update(update) {
      const patch = typeof update === "function" ? update(state) : update;
      state = Object.freeze({ ...state, ...patch });
      for (const subscriber of subscribers) {
        subscriber(state);
      }
    },
  });
}

function boot() {
  dom = findDom();
  installPlanPinControl();
  installMapTab();
  store.subscribe(renderApp);
  bindEvents();
  bindUnexpectedErrors();
  renderApp(store.getState());
  void refreshSchema();
}

function installMapTab() {
  const tab = document.createElement("button");
  tab.id = "map-tab";
  tab.className = "tab";
  tab.type = "button";
  tab.setAttribute("role", "tab");
  tab.dataset.view = "Map";
  tab.textContent = "Map";
  tab.hidden = true;

  const graphIndex = dom.tabs.findIndex((item) => item.dataset.view === "Graph");
  const insertAt = graphIndex < 0 ? dom.tabs.length : graphIndex;
  const sibling = dom.tabs[insertAt];
  if (sibling) {
    sibling.before(tab);
  } else {
    document.querySelector("[aria-label='Result views']")?.append(tab);
  }
  dom.tabs.splice(insertAt, 0, tab);
  dom.mapTab = tab;
}

function findDom() {
  const byId = (id) => {
    const element = document.getElementById(id);
    if (!element) {
      throw new Error(`Missing application element #${id}.`);
    }
    return element;
  };

  const planHeading = byId("plan-heading");
  const planHeader = planHeading.closest(".panel-header");
  if (!planHeader) {
    throw new Error("Missing plan panel header.");
  }

  return {
    editor: byId("query-editor"),
    autocomplete: byId("query-autocomplete"),
    editorError: byId("editor-error"),
    explainButton: byId("explain-button"),
    runButton: byId("run-button"),
    plan: byId("plan-view"),
    planHeader,
    planSourceBadge: byId("plan-source-badge"),
    resultErrors: byId("result-error"),
    results: byId("results-view"),
    schema: byId("schema-view"),
    tabs: Array.from(document.querySelectorAll("[data-view]")),
  };
}

function installPlanPinControl() {
  const control = document.createElement("div");
  control.className = "plan-pin-control";

  const trigger = document.createElement("button");
  trigger.className = "button plan-pin-trigger";
  trigger.type = "button";
  trigger.textContent = "Pin";

  const form = document.createElement("form");
  form.className = "plan-pin-form";
  form.hidden = true;

  const input = document.createElement("input");
  input.className = "plan-pin-name";
  input.type = "text";
  input.autocomplete = "off";
  input.placeholder = "Pin name";
  input.setAttribute("aria-label", "Pinned plan name");

  const save = document.createElement("button");
  save.className = "button primary plan-pin-save";
  save.type = "submit";
  save.textContent = "Save";

  const cancel = document.createElement("button");
  cancel.className = "button plan-pin-cancel";
  cancel.type = "button";
  cancel.textContent = "Cancel";

  const error = document.createElement("span");
  error.className = "plan-pin-error";
  error.setAttribute("role", "alert");
  error.hidden = true;

  form.append(input, save, cancel, error);
  control.append(trigger, form);
  dom.planHeader.append(control);
  Object.assign(dom, {
    pinControl: control,
    pinTrigger: trigger,
    pinForm: form,
    pinName: input,
    pinSave: save,
    pinCancel: cancel,
    pinError: error,
  });
}

function bindEvents() {
  dom.editor.addEventListener("input", (event) => {
    store.update({
      editorText: event.target.value,
      editorError: null,
      treeError: null,
      treeEditsActive: false,
    });
    refreshAutocomplete();
  });
  dom.editor.addEventListener("keydown", handleEditorKeydown);
  dom.editor.addEventListener("keyup", (event) => {
    if (!autocompleteOpen() && CARET_NAVIGATION_KEYS.has(event.key)) {
      refreshAutocomplete();
    }
  });
  dom.editor.addEventListener("click", refreshAutocomplete);
  dom.editor.addEventListener("focus", refreshAutocomplete);
  dom.editor.addEventListener("blur", dismissAutocomplete);
  dom.autocomplete.addEventListener("pointerdown", (event) => event.preventDefault());
  dom.autocomplete.addEventListener("click", (event) => {
    const option = event.target.closest("[data-autocomplete-index]");
    if (option) {
      acceptAutocomplete(Number(option.dataset.autocompleteIndex));
    }
  });
  dom.editorError.addEventListener("click", (event) => {
    const hint = event.target.closest("[data-noparse-hint-index]");
    if (hint) {
      insertNoParseHint(Number(hint.dataset.noparseHintIndex));
    }
  });
  dom.explainButton.addEventListener("click", () => void explainCurrentInput());
  dom.runButton.addEventListener("click", () => void runCurrentInput());
  dom.pinTrigger.addEventListener("click", openPinName);
  dom.pinForm.addEventListener("submit", (event) => {
    event.preventDefault();
    void pinCurrentExplanation();
  });
  dom.pinName.addEventListener("input", (event) => {
    store.update({ pinName: event.target.value, pinError: null });
  });
  dom.pinName.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      closePinName();
    }
  });
  dom.pinCancel.addEventListener("click", closePinName);
  for (const tab of dom.tabs) {
    tab.addEventListener("click", () => void selectResultView(tab.dataset.view));
    tab.addEventListener("keydown", handleResultTabKeydown);
  }
}

async function selectResultView(name) {
  store.update({ activeResultView: name });
  if (name === "Grid") {
    await ensureGridRows();
  } else if (name === "Map") {
    await ensureMapRows();
  }
}

function handleResultTabKeydown(event) {
  const tabs = visibleResultTabs();
  const index = tabs.indexOf(event.currentTarget);
  let next = null;
  if (event.key === "ArrowRight" || event.key === "ArrowDown") {
    next = (index + 1) % tabs.length;
  } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
    next = (index - 1 + tabs.length) % tabs.length;
  } else if (event.key === "Home") {
    next = 0;
  } else if (event.key === "End") {
    next = tabs.length - 1;
  }
  if (next !== null) {
    event.preventDefault();
    tabs[next].focus();
    void selectResultView(tabs[next].dataset.view);
  }
}

function visibleResultTabs() {
  return dom.tabs.filter((tab) => !tab.hidden);
}

function handleEditorKeydown(event) {
  if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
    event.preventDefault();
    dismissAutocomplete();
    void runCurrentInput();
    return;
  }
  if (event.isComposing || !autocompleteOpen()) {
    return;
  }

  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const offset = event.key === "ArrowDown" ? 1 : -1;
    selectAutocomplete(autocomplete.activeIndex + offset);
  } else if (event.key === "Tab" || event.key === "Enter") {
    event.preventDefault();
    acceptAutocomplete(autocomplete.activeIndex);
  } else if (event.key === "Escape") {
    event.preventDefault();
    dismissAutocomplete();
  }
}

function refreshAutocomplete() {
  const state = store.getState();
  if (!state.schema || uiBusy(state) || document.activeElement !== dom.editor) {
    dismissAutocomplete();
    return;
  }
  const caret = dom.editor.selectionStart ?? state.editorText.length;
  autocomplete = autocompleteFor(state.editorText, caret, state.schema);
  renderAutocomplete();
}

function autocompleteFor(text, caret, schema) {
  const beforeCaret = text.slice(0, caret);
  const columnMatch = beforeCaret.match(
    /(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*)\.(`(?:\\.|[^`])*|[A-Za-z_][A-Za-z0-9_]*)?$/,
  );
  if (columnMatch) {
    const columns = bindingColumns(beforeCaret, decodeIdentifier(columnMatch[1]), schema);
    return autocompleteResult(columns, columnMatch[2] ?? "", caret, true);
  }

  const nodeMatch = beforeCaret.match(
    /\bnodes\s*\(\s*(`(?:\\.|[^`])*|[A-Za-z_][A-Za-z0-9_]*)?$/,
  );
  if (nodeMatch) {
    return autocompleteResult(schema.node_tables, nodeMatch[1] ?? "", caret, true);
  }

  const expandMatch = beforeCaret.match(
    /\bexpand\s+(`(?:\\.|[^`])*|[A-Za-z_][A-Za-z0-9_]*)?$/,
  );
  if (expandMatch) {
    return autocompleteResult(schema.rel_tables, expandMatch[1] ?? "", caret, true);
  }

  const stageMatch = beforeCaret.match(/\|\s+([A-Za-z]*)$/);
  if (stageMatch) {
    return autocompleteResult(STAGE_KEYWORDS, stageMatch[1], caret, false);
  }
  return emptyAutocomplete();
}

function autocompleteResult(candidates, rawPrefix, caret, identifier) {
  const prefix = identifier ? identifierFragment(rawPrefix) : rawPrefix;
  const foldedPrefix = asciiFold(prefix);
  const suggestions = candidates
    .map((candidate) => typeof candidate === "string" ? candidate : candidate?.name)
    .filter((candidate) => typeof candidate === "string"
      && asciiFold(candidate).startsWith(foldedPrefix))
    .map((candidate) => ({
      label: candidate,
      insertion: identifier ? formatIdentifier(candidate) : candidate,
    }));
  if (suggestions.length === 0) {
    return emptyAutocomplete();
  }
  return {
    suggestions,
    activeIndex: 0,
    replaceStart: caret - rawPrefix.length,
    replaceEnd: caret,
  };
}

function bindingColumns(text, binding, schema) {
  const declarations = bindingDeclarations(text, schema);
  const tableName = declarations.get(asciiFold(binding));
  const table = schema.node_tables.find(
    (candidate) => asciiFold(candidate.name) === asciiFold(tableName ?? ""),
  );
  return table?.columns ?? [];
}

function bindingDeclarations(text, schema) {
  const declarations = [];
  const nodes = /\bnodes\s*\(\s*(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*)\s*\)\s+as\s+(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*)/g;
  const expands = /\bexpand\s+(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*)\s+(out|in|both)(?:\s+from\s+(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*))?\s+as\s+(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*)/g;
  for (const match of text.matchAll(nodes)) {
    const table = findByFold(schema.node_tables, decodeIdentifier(match[1]));
    if (table) {
      declarations.push({ index: match.index, binding: decodeIdentifier(match[2]), table: table.name });
    }
  }
  for (const match of text.matchAll(expands)) {
    const rel = findByFold(schema.rel_tables, decodeIdentifier(match[1]));
    const table = relTarget(rel, match[2]);
    if (table) {
      declarations.push({ index: match.index, binding: decodeIdentifier(match[4]), table });
    }
  }
  declarations.sort((left, right) => left.index - right.index);
  return new Map(declarations.map(({ binding, table }) => [asciiFold(binding), table]));
}

function relTarget(rel, direction) {
  if (!rel) {
    return null;
  }
  if (direction === "out") {
    return rel.to;
  }
  if (direction === "in") {
    return rel.from;
  }
  return asciiFold(rel.from) === asciiFold(rel.to) ? rel.to : null;
}

function findByFold(candidates, name) {
  const foldedName = asciiFold(name);
  return candidates.find((candidate) => asciiFold(candidate.name) === foldedName);
}

function asciiFold(value) {
  return value.replace(/[A-Z]/g, (character) => (
    String.fromCharCode(character.charCodeAt(0) + 32)
  ));
}

function identifierFragment(raw) {
  return raw.startsWith("`") ? decodeEscapes(raw.slice(1)) : raw;
}

function decodeIdentifier(raw) {
  return raw.startsWith("`") ? decodeEscapes(raw.slice(1, -1)) : raw;
}

function decodeEscapes(value) {
  return value.replace(/\\(?:`|\\|n|r|t|u\{[0-9A-Fa-f]{1,6}\})/g, (escape) => {
    if (escape.startsWith("\\u{")) {
      return String.fromCodePoint(Number.parseInt(escape.slice(3, -1), 16));
    }
    return { "\\`": "`", "\\\\": "\\", "\\n": "\n", "\\r": "\r", "\\t": "\t" }[escape];
  });
}

function formatIdentifier(name) {
  if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) && !RESERVED_WORDS.has(name)) {
    return name;
  }
  const escaped = name
    .replace(/\\/g, "\\\\")
    .replace(/`/g, "\\`")
    .replace(/\n/g, "\\n")
    .replace(/\r/g, "\\r")
    .replace(/\t/g, "\\t");
  return `\`${escaped}\``;
}

function selectAutocomplete(index) {
  const length = autocomplete.suggestions.length;
  autocomplete.activeIndex = (index + length) % length;
  renderAutocomplete();
  document.getElementById(autocompleteOptionId(autocomplete.activeIndex))
    ?.scrollIntoView({ block: "nearest" });
}

function acceptAutocomplete(index) {
  const suggestion = autocomplete.suggestions[index];
  if (!suggestion) {
    return;
  }
  const state = store.getState();
  const editorText = state.editorText.slice(0, autocomplete.replaceStart)
    + suggestion.insertion
    + state.editorText.slice(autocomplete.replaceEnd);
  const caret = autocomplete.replaceStart + suggestion.insertion.length;
  autocomplete = emptyAutocomplete();
  store.update({ editorText, editorError: null, treeError: null, treeEditsActive: false });
  renderAutocomplete();
  focusEditor(caret);
}

function dismissAutocomplete() {
  if (!dom || (!autocompleteOpen() && dom.autocomplete.hidden)) {
    return;
  }
  autocomplete = emptyAutocomplete();
  renderAutocomplete();
}

function renderAutocomplete() {
  const open = autocompleteOpen();
  dom.autocomplete.replaceChildren();
  dom.autocomplete.hidden = !open;
  dom.editor.setAttribute("aria-expanded", String(open));
  if (!open) {
    dom.editor.removeAttribute("aria-activedescendant");
    return;
  }
  autocomplete.suggestions.forEach((suggestion, index) => {
    const option = document.createElement("li");
    option.id = autocompleteOptionId(index);
    option.className = "autocomplete-option";
    option.setAttribute("role", "option");
    option.setAttribute("aria-selected", String(index === autocomplete.activeIndex));
    option.dataset.autocompleteIndex = String(index);
    option.textContent = suggestion.label;
    dom.autocomplete.append(option);
  });
  dom.editor.setAttribute(
    "aria-activedescendant",
    autocompleteOptionId(autocomplete.activeIndex),
  );
}

function autocompleteOptionId(index) {
  return `query-autocomplete-option-${index}`;
}

function autocompleteOpen() {
  return autocomplete.suggestions.length > 0;
}

function emptyAutocomplete() {
  return { suggestions: [], activeIndex: -1, replaceStart: 0, replaceEnd: 0 };
}

function bindUnexpectedErrors() {
  window.addEventListener("error", (event) => {
    addResultError(`Unexpected UI error: ${event.message}`);
  });
  window.addEventListener("unhandledrejection", (event) => {
    event.preventDefault();
    addResultError(`Unexpected UI error: ${errorMessage(event.reason)}`);
  });
}

function renderApp(state) {
  renderEditor(state);
  renderPlan(state);
  renderResults(state);
  renderSchema(state);
}

function renderEditor(state) {
  if (dom.editor.value !== state.editorText) {
    dom.editor.value = state.editorText;
  }
  const busy = uiBusy(state);
  dom.editor.readOnly = busy;
  dom.explainButton.disabled = busy;
  dom.runButton.disabled = busy;
  dom.explainButton.textContent = state.busyAction === "explain" ? "Explaining…" : "Explain";
  dom.runButton.textContent = state.busyAction === "run" ? "Running…" : "Run";
  renderEditorError(state.editorError);
}

function renderEditorError(error) {
  dom.editorError.replaceChildren();
  dom.editorError.hidden = error === null;
  if (error === null) {
    return;
  }
  if (error.kind === "noparse") {
    renderNoParseError(error);
    return;
  }

  if (error.position !== null) {
    const position = document.createElement("span");
    position.className = "error-position";
    position.textContent = `position ${error.position}`;
    dom.editorError.append(position);
  }
  dom.editorError.append(document.createTextNode(error.message));
}

function renderNoParseError(error) {
  const refusal = document.createElement("div");
  refusal.className = "noparse-refusal";

  const message = document.createElement("p");
  message.className = "noparse-message";
  message.textContent = "I couldn't match that request to a safe plan.";
  refusal.append(message);

  appendUnrecognizedTokens(refusal, error.report.unrecognized);
  appendNearestPhrasings(refusal, error.report.nearest);
  appendParserDetail(refusal, error.parserDetail);
  dom.editorError.append(refusal);
}

function appendUnrecognizedTokens(container, unrecognized) {
  if (unrecognized.length === 0) {
    return;
  }
  container.append(noParseLabel("Unrecognized"));
  const list = document.createElement("ul");
  list.className = "noparse-token-list";
  for (const item of unrecognized) {
    const row = document.createElement("li");
    row.append(codeText(item.token));
    if (item.suggestion === null) {
      row.append(document.createTextNode(" — no close match"));
    } else {
      row.append(document.createTextNode(" → try "), codeText(item.suggestion));
    }
    list.append(row);
  }
  container.append(list);
}

function appendNearestPhrasings(container, nearest) {
  if (nearest.length === 0) {
    return;
  }
  container.append(noParseLabel("Closest working phrasings"));
  const hints = document.createElement("div");
  hints.className = "noparse-hints";
  nearest.forEach((hint, index) => {
    const button = document.createElement("button");
    button.className = "noparse-hint-button";
    button.type = "button";
    button.dataset.noparseHintIndex = String(index);
    button.textContent = hint.example;
    hints.append(button);
  });
  container.append(hints);
}

function appendParserDetail(container, parserDetail) {
  const details = document.createElement("details");
  details.className = "devonplan-parse-detail";
  const summary = document.createElement("summary");
  summary.textContent = "DevonPlan parse detail";
  const message = document.createElement("pre");
  message.textContent = parserDetail;
  details.append(summary, message);
  container.append(details);
}

function noParseLabel(text) {
  const label = document.createElement("strong");
  label.className = "noparse-label";
  label.textContent = text;
  return label;
}

function codeText(text) {
  const code = document.createElement("code");
  code.textContent = text;
  return code;
}

function renderPlan(state) {
  dom.planSourceBadge.hidden = !state.compiledFromNaturalLanguage || !state.explanation;
  renderPinControl(state);
  getView("Plan").render(dom.plan, {
    explanation: state.explanation,
    schema: state.schema,
    loading: state.busyAction !== null,
    treeBusyNode: state.treeBusyNode,
    treeError: state.treeError,
    actions: {
      mutateTree,
    },
  });
}

function renderPinControl(state) {
  const queryReady = state.explanation?.kind === "query";
  const busy = uiBusy(state);
  dom.pinTrigger.hidden = state.pinNameOpen;
  dom.pinTrigger.disabled = busy || !queryReady;
  dom.pinTrigger.title = queryReady
    ? "Pin this engine-confirmed plan"
    : "Explain a query to pin its engine-confirmed plan";

  dom.pinForm.hidden = !state.pinNameOpen;
  dom.pinName.disabled = busy;
  dom.pinSave.disabled = busy || state.pinName.trim().length === 0;
  dom.pinCancel.disabled = busy;
  dom.pinSave.textContent = state.busyAction === "pin" ? "Saving…" : "Save";
  if (dom.pinName.value !== state.pinName) {
    dom.pinName.value = state.pinName;
  }
  dom.pinError.hidden = state.pinError === null;
  dom.pinError.textContent = state.pinError ?? "";
}

function renderResults(state) {
  dom.resultErrors.replaceChildren();
  for (const error of state.resultErrors) {
    const message = document.createElement("div");
    message.className = "result-error";
    message.setAttribute("role", "alert");
    message.textContent = error;
    dom.resultErrors.append(message);
  }

  const mapContext = mapViewContext(state.schema, state.explanation);
  dom.mapTab.hidden = mapContext === null;
  const activeView = state.activeResultView === "Map" && mapContext === null
    ? "Table"
    : state.activeResultView;
  for (const tab of dom.tabs) {
    const selected = tab.dataset.view === activeView;
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
  }
  getView(activeView).render(dom.results, {
    result: state.result,
    resultCanonical: state.resultCanonical,
    explanation: state.explanation,
    loading: state.busyAction === "run",
    schema: state.schema,
    schemaLoading: state.schemaLoading,
    gridBusy: uiBusy(state),
    grid: {
      table: state.gridTable,
      sort: state.gridSort,
      filters: state.gridFilters,
      offset: state.gridOffset,
      resultTable: state.gridResultTable,
    },
    map: {
      table: mapContext?.table ?? null,
      column: mapContext?.column ?? null,
      result: state.mapResult,
      resultTable: state.mapResultTable,
      resultColumn: state.mapResultColumn,
      loading: state.mapBusy,
      error: state.mapError,
    },
    actions: {
      runGrid: runGridRequest,
    },
  });
}

function renderSchema(state) {
  getView("Schema").render(dom.schema, {
    schema: state.schema,
    history: state.history,
    loading: state.schemaLoading,
    busy: uiBusy(state),
    actions: {
      insertStarter,
      selectHistory,
      runPin: runPinnedPlan,
      unpin: unpinPlan,
    },
  });
}

function getView(name) {
  const view = views.get(name);
  if (!view) {
    throw new Error(`View ${name} is not registered.`);
  }
  return view;
}

function openPinName() {
  const state = store.getState();
  if (uiBusy(state) || state.explanation?.kind !== "query") {
    return;
  }
  store.update({ pinNameOpen: true, pinName: "", pinError: null });
  dom.pinName.focus();
}

function closePinName() {
  if (store.getState().busyAction === "pin") {
    return;
  }
  store.update({ pinNameOpen: false, pinName: "", pinError: null });
  dom.pinTrigger.focus();
}

async function pinCurrentExplanation() {
  const state = store.getState();
  const name = state.pinName.trim();
  if (uiBusy(state)
    || state.explanation?.kind !== "query"
    || typeof state.explanationText !== "string"
    || name.length === 0) {
    return;
  }

  store.update({ busyAction: "pin", pinError: null });
  try {
    const response = await requestJson("/api/pin", {
      method: "POST",
      body: {
        name,
        plan: state.explanation.plan,
        text: state.explanationText,
      },
    });
    if (response?.ok !== true) {
      throw new Error("The server returned a malformed pin response.");
    }
    store.update({
      busyAction: null,
      pinNameOpen: false,
      pinName: "",
      pinError: null,
    });
    await refreshSchema();
  } catch (error) {
    store.update({ busyAction: null, pinError: errorMessage(error) });
  }
}

async function explainCurrentInput() {
  const text = store.getState().editorText;
  if (!prepareInput(text, "explain")) {
    return;
  }

  try {
    const resolved = await resolveInput(text);
    if (resolved.noparse) {
      surfaceNoParse(resolved.noparse);
      return;
    }
    store.update({
      explanation: resolved.explanation,
      explanationText: text,
      busyAction: null,
      treeError: null,
      treeEditsActive: false,
      compiledFromNaturalLanguage: resolved.compiledFromNaturalLanguage,
    });
  } catch (error) {
    surfaceExplainFailure(error);
  }
}

async function runCurrentInput() {
  const state = store.getState();
  if (state.treeEditsActive && state.explanation?.kind === "query") {
    await runConfirmedTree(state.explanation);
    return;
  }

  const text = state.editorText;
  if (!prepareInput(text, "run")) {
    return;
  }

  let phase = "explain";
  try {
    const resolved = await resolveInput(text);
    if (resolved.noparse) {
      surfaceNoParse(resolved.noparse);
      return;
    }
    const explanation = resolved.explanation;
    store.update({
      explanation,
      explanationText: text,
      treeEditsActive: false,
      compiledFromNaturalLanguage: resolved.compiledFromNaturalLanguage,
    });
    await nextPaint();

    phase = "execute";
    const result = await executeExplanation(explanation);
    finishExecution(text, explanation, result);
    if (explanation.kind === "statement") {
      void refreshSchema();
    }
  } catch (error) {
    surfaceRunFailure(error, phase);
  }
}

async function runConfirmedTree(explanation) {
  if (uiBusy(store.getState())) {
    return;
  }
  dismissAutocomplete();
  store.update({
    result: null,
    editorError: null,
    resultErrors: [],
    busyAction: "run",
    treeError: null,
    gridResultTable: null,
  });
  try {
    await nextPaint();
    const result = await executeExplanation(explanation);
    finishExecution(explanation.canonical, explanation, result);
  } catch (error) {
    surfaceRunFailure(error, "execute");
  }
}

function prepareInput(text, action) {
  if (uiBusy(store.getState())) {
    return false;
  }
  if (text.trim().length === 0) {
    store.update({
      explanation: null,
      explanationText: null,
      editorError: { message: "Enter a question or DevonPlan query or statement.", position: 1 },
      compiledFromNaturalLanguage: false,
      pinNameOpen: false,
      pinName: "",
      pinError: null,
    });
    return false;
  }

  dismissAutocomplete();
  store.update({
    explanation: null,
    explanationText: null,
    result: action === "run" ? null : store.getState().result,
    editorError: null,
    resultErrors: [],
    busyAction: action,
    treeError: null,
    treeEditsActive: false,
    compiledFromNaturalLanguage: false,
    pinNameOpen: false,
    pinName: "",
    pinError: null,
    gridResultTable: action === "run" ? null : store.getState().gridResultTable,
  });
  return true;
}

async function resolveInput(text) {
  const response = await ask(text);
  if (validExplanation(response)) {
    return {
      explanation: response,
      compiledFromNaturalLanguage: true,
      noparse: null,
    };
  }
  try {
    return {
      explanation: await explain(text),
      compiledFromNaturalLanguage: false,
      noparse: null,
    };
  } catch (error) {
    const parserDetail = errorMessage(error);
    if (parsePosition(parserDetail) === null) {
      throw error;
    }
    return {
      explanation: null,
      compiledFromNaturalLanguage: false,
      noparse: { report: response.noparse, parserDetail },
    };
  }
}

async function explain(text) {
  const response = await requestJson("/api/explain", {
    method: "POST",
    body: { text },
  });
  if (!validExplanation(response)) {
    throw new Error("The server returned a malformed explain response.");
  }
  return response;
}

async function ask(text) {
  const response = await requestJson("/api/ask", {
    method: "POST",
    body: { text },
  });
  if (!validExplanation(response) && !validNoParseResponse(response)) {
    throw new Error("The server returned a malformed ask response.");
  }
  return response;
}

async function explainPlan(plan) {
  const response = await requestJson("/api/explain", {
    method: "POST",
    body: { plan },
  });
  if (!validExplanation(response) || response.kind !== "query") {
    throw new Error("The server returned a malformed query explain response.");
  }
  return response;
}

async function mutateTree(plan, nodeId, sourceText = null) {
  const state = store.getState();
  if (uiBusy(state)) {
    addResultError("Wait for the current request to finish before editing the query tree.");
    return;
  }
  const explanationText = sourceText ?? state.explanationText ?? state.editorText;
  dismissAutocomplete();
  store.update({ treeBusyNode: nodeId, treeError: null });
  try {
    const explanation = await explainPlan(plan);
    store.update({
      explanation,
      explanationText,
      treeBusyNode: null,
      treeError: null,
      treeEditsActive: true,
    });
  } catch (error) {
    store.update({
      treeBusyNode: null,
      treeError: { nodeId, message: errorMessage(error) },
    });
  }
}

async function executeExplanation(explanation) {
  if (explanation.kind === "statement") {
    const response = await requestJson("/api/statement", {
      method: "POST",
      body: { statement: explanation.plan },
    });
    if (response?.ok !== true) {
      throw new Error("The server returned a malformed statement response.");
    }
    return { kind: "statement", data: response };
  }

  const response = await requestJson("/api/query", {
    method: "POST",
    body: { plan: explanation.plan },
  });
  return { kind: "query", data: response };
}

function finishExecution(text, explanation, result, gridTable = null) {
  const history = appendHistory(store.getState().history, text, explanation);
  store.update({
    result,
    resultCanonical: explanation.canonical,
    history: history.entries,
    resultErrors: history.error ? [history.error] : [],
    busyAction: null,
    gridResultTable: gridTable,
  });
  void ensureMapRows();
}

function surfaceExplainFailure(error) {
  const message = errorMessage(error);
  const position = parsePosition(message);
  if (position !== null) {
    store.update({
      explanation: null,
      explanationText: null,
      editorError: { message, position },
      busyAction: null,
      compiledFromNaturalLanguage: false,
    });
    return;
  }
  store.update({ resultErrors: [message], busyAction: null });
}

function surfaceRunFailure(error, phase) {
  const message = errorMessage(error);
  const position = phase === "explain" ? parsePosition(message) : null;
  if (position !== null) {
    store.update({
      explanation: null,
      explanationText: null,
      editorError: { message, position },
      busyAction: null,
      compiledFromNaturalLanguage: false,
    });
    return;
  }
  store.update({ resultErrors: [message], busyAction: null });
}

function surfaceNoParse(noparse) {
  store.update({
    explanation: null,
    explanationText: null,
    editorError: { kind: "noparse", ...noparse },
    busyAction: null,
    treeError: null,
    treeEditsActive: false,
    compiledFromNaturalLanguage: false,
    pinNameOpen: false,
    pinName: "",
    pinError: null,
  });
}

async function runPinnedPlan(pin) {
  const state = store.getState();
  if (uiBusy(state)) {
    addResultError("Wait for the current request to finish before running a pinned plan.");
    return;
  }
  dismissAutocomplete();
  store.update({
    explanation: null,
    explanationText: null,
    result: null,
    editorError: null,
    resultErrors: [],
    busyAction: "run",
    treeError: null,
    treeEditsActive: false,
    compiledFromNaturalLanguage: false,
    pinNameOpen: false,
    pinName: "",
    pinError: null,
    gridResultTable: null,
  });

  let phase = "explain";
  try {
    const explanation = await explain(pin.canonical);
    if (explanation.kind !== "query") {
      throw new Error("The pinned plan did not explain as a query.");
    }
    store.update({ explanation, explanationText: pin.text });
    await nextPaint();
    phase = "execute";
    const response = await requestJson("/api/run-pin", {
      method: "POST",
      body: { name: pin.name },
    });
    finishExecution(pin.text, explanation, { kind: "query", data: response });
  } catch (error) {
    surfaceRunFailure(error, phase);
  }
}

async function unpinPlan(name) {
  if (uiBusy(store.getState())) {
    addResultError("Wait for the current request to finish before removing a pinned plan.");
    return;
  }
  store.update({ resultErrors: [], busyAction: "unpin" });
  try {
    const response = await requestJson("/api/unpin", {
      method: "POST",
      body: { name },
    });
    if (response?.ok !== true) {
      throw new Error("The server returned a malformed unpin response.");
    }
    store.update({ busyAction: null });
    await refreshSchema();
  } catch (error) {
    store.update({ resultErrors: [errorMessage(error)], busyAction: null });
  }
}

async function refreshSchema() {
  store.update({ schemaLoading: true });
  try {
    const schema = await requestJson("/api/schema");
    if (!validSchema(schema)) {
      throw new Error("The server returned a malformed schema response.");
    }
    mapRequestVersion += 1;
    store.update({
      schema,
      schemaLoading: false,
      gridResultTable: null,
      mapTable: null,
      mapColumn: null,
      mapResult: null,
      mapResultTable: null,
      mapResultColumn: null,
      mapBusy: false,
      mapError: null,
    });
    refreshAutocomplete();
    void ensureGridRows();
    void ensureMapRows();
  } catch (error) {
    store.update({ schemaLoading: false });
    addResultError(errorMessage(error));
  }
}

async function ensureMapRows() {
  const state = store.getState();
  const context = mapViewContext(state.schema, state.explanation);
  if (state.activeResultView !== "Map"
    || state.schemaLoading
    || uiBusy(state)
    || context === null) {
    return;
  }
  const request = createMapRequest(context);
  if (!request) {
    return;
  }
  const sameTarget = sameFold(state.mapTable, request.table)
    && sameFold(state.mapColumn, request.column);
  const loaded = sameFold(state.mapResultTable, request.table)
    && sameFold(state.mapResultColumn, request.column)
    && validQueryResult(state.mapResult);
  if (loaded || (state.mapBusy && sameTarget)) {
    return;
  }
  await runMapRequest(request);
}

async function runMapRequest(request) {
  const version = mapRequestVersion + 1;
  mapRequestVersion = version;
  store.update({
    mapTable: request.table,
    mapColumn: request.column,
    mapResult: null,
    mapResultTable: null,
    mapResultColumn: null,
    mapBusy: true,
    mapError: null,
  });
  try {
    // This internal hydration query does not change the user's confirmed plan;
    // it supplies geographic context in the same way `/api/graph` supplies a neighborhood.
    const result = await requestJson("/api/query", {
      method: "POST",
      body: { plan: request.plan },
    });
    if (!validQueryResult(result)) {
      throw new Error("The server returned a malformed map result.");
    }
    if (version !== mapRequestVersion || !currentMapTarget(request)) {
      return;
    }
    store.update({
      mapResult: result,
      mapResultTable: request.table,
      mapResultColumn: request.column,
      mapBusy: false,
      mapError: null,
    });
  } catch (error) {
    if (version === mapRequestVersion) {
      store.update({ mapBusy: false, mapError: errorMessage(error) });
    }
  }
}

function currentMapTarget(request) {
  const state = store.getState();
  const context = mapViewContext(state.schema, state.explanation);
  return context !== null
    && sameFold(context.table.name, request.table)
    && sameFold(context.column.name, request.column);
}

async function ensureGridRows() {
  const state = store.getState();
  if (state.activeResultView !== "Grid"
    || state.schemaLoading
    || uiBusy(state)
    || !validSchema(state.schema)) {
    return;
  }
  const request = createGridRequest(state.schema, {
    table: state.gridTable,
    sort: state.gridSort,
    filters: state.gridFilters,
    offset: state.gridOffset,
  });
  if (!request) {
    return;
  }
  const currentTable = asciiFold(state.gridResultTable ?? "");
  if (currentTable === asciiFold(request.table) && state.result?.kind === "query") {
    return;
  }
  await runGridRequest(request);
}

async function runGridRequest(request) {
  const state = store.getState();
  if (uiBusy(state)) {
    addResultError("Wait for the current request to finish before changing the grid.");
    return;
  }
  const table = findByFold(state.schema?.node_tables ?? [], request?.table ?? "");
  if (!table || !validGridRequest(request)) {
    addResultError("The grid could not build a valid class plan.");
    return;
  }

  dismissAutocomplete();
  store.update({
    explanation: null,
    explanationText: null,
    result: null,
    editorError: null,
    resultErrors: [],
    busyAction: "run",
    treeError: null,
    treeEditsActive: false,
    compiledFromNaturalLanguage: false,
    pinNameOpen: false,
    pinName: "",
    pinError: null,
    gridTable: table.name,
    gridSort: request.sort,
    gridFilters: request.filters,
    gridOffset: request.offset,
    gridResultTable: null,
  });

  let phase = "explain";
  try {
    const explanation = await explainPlan(request.plan);
    store.update({
      explanation,
      explanationText: explanation.canonical,
      treeError: null,
      treeEditsActive: false,
    });
    await nextPaint();
    phase = "execute";
    const result = await executeExplanation(explanation);
    finishExecution(explanation.canonical, explanation, result, table.name);
  } catch (error) {
    surfaceRunFailure(error, phase);
  }
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

function appendHistory(entries, text, explanation) {
  const entry = {
    input: text,
    canonical: explanation.canonical,
    kind: explanation.kind,
    executed_at: new Date().toISOString(),
  };
  const nextEntries = [entry, ...entries];
  try {
    window.localStorage.setItem(HISTORY_KEY, JSON.stringify(nextEntries));
    return { entries: nextEntries, error: null };
  } catch (error) {
    return {
      entries: nextEntries,
      error: `Executed successfully, but session history could not be saved: ${errorMessage(error)}`,
    };
  }
}

function readHistory() {
  try {
    const value = window.localStorage.getItem(HISTORY_KEY);
    if (value === null) {
      return { entries: [], error: null };
    }
    const parsed = JSON.parse(value);
    if (!Array.isArray(parsed)) {
      return malformedHistory();
    }
    const entries = parsed.filter(validHistoryEntry);
    const error = entries.length === parsed.length
      ? null
      : "Some session history entries were malformed and could not be displayed.";
    return { entries, error };
  } catch (error) {
    return {
      entries: [],
      error: `Session history could not be loaded: ${errorMessage(error)}`,
    };
  }
}

function malformedHistory() {
  return {
    entries: [],
    error: "Session history could not be loaded because its stored value is malformed.",
  };
}

function validHistoryEntry(entry) {
  return entry !== null
    && typeof entry === "object"
    && typeof entry.input === "string"
    && (entry.kind === "query" || entry.kind === "statement");
}

function insertStarter(starter) {
  if (!editorActionAvailable()) {
    return;
  }
  const state = store.getState();
  const start = dom.editor.selectionStart ?? state.editorText.length;
  const end = dom.editor.selectionEnd ?? start;
  const editorText = state.editorText.slice(0, start) + starter + state.editorText.slice(end);
  dismissAutocomplete();
  store.update({
    editorText,
    editorError: null,
    treeError: null,
    treeEditsActive: false,
    compiledFromNaturalLanguage: false,
  });
  focusEditor(start + starter.length);
  void seedTreeFromStarter(starter);
}

function insertNoParseHint(index) {
  if (!editorActionAvailable()) {
    return;
  }
  const error = store.getState().editorError;
  const hint = error?.kind === "noparse" ? error.report.nearest[index]?.example : null;
  if (typeof hint !== "string") {
    return;
  }
  dismissAutocomplete();
  store.update({
    editorText: hint,
    editorError: null,
    treeError: null,
    treeEditsActive: false,
    compiledFromNaturalLanguage: false,
  });
  focusEditor(hint.length);
}

async function seedTreeFromStarter(starter) {
  const match = starter.match(
    /^nodes\s*\(\s*(`(?:\\.|[^`])*`|[A-Za-z_][A-Za-z0-9_]*)\s*\)/,
  );
  const tableName = match ? decodeIdentifier(match[1]) : null;
  const table = findByFold(store.getState().schema?.node_tables ?? [], tableName ?? "");
  if (!table) {
    addResultError("The schema starter could not be matched to a node table.");
    return;
  }
  await mutateTree({
    v: 0,
    plan: {
      op: "ScanNodes",
      table: table.name,
      binding: asciiFold(table.name).replace(/\./g, "_"),
    },
  }, "node-0", starter);
}

function selectHistory(input) {
  if (!editorActionAvailable()) {
    return;
  }
  dismissAutocomplete();
  store.update({
    editorText: input,
    editorError: null,
    treeError: null,
    treeEditsActive: false,
  });
  focusEditor(input.length);
}

function editorActionAvailable() {
  if (!uiBusy(store.getState())) {
    return true;
  }
  addResultError("Wait for the current request to finish before changing the editor.");
  return false;
}

function uiBusy(state) {
  return state.busyAction !== null || state.treeBusyNode !== null;
}

function focusEditor(position) {
  dom.editor.focus();
  dom.editor.setSelectionRange(position, position);
}

function addResultError(message) {
  store.update((state) => ({ resultErrors: [...state.resultErrors, message] }));
}

function validExplanation(value) {
  return value !== null
    && typeof value === "object"
    && (value.kind === "query" || value.kind === "statement")
    && typeof value.canonical === "string"
    && value.plan !== null
    && typeof value.plan === "object";
}

function validGridRequest(value) {
  return value !== null
    && typeof value === "object"
    && typeof value.table === "string"
    && (value.sort === null || typeof value.sort === "object")
    && Array.isArray(value.filters)
    && Number.isSafeInteger(value.offset)
    && value.offset >= 0
    && value.plan !== null
    && typeof value.plan === "object"
    && value.plan.v === 0
    && value.plan.plan !== null
    && typeof value.plan.plan === "object";
}

function validNoParseResponse(value) {
  const report = value?.noparse;
  return report !== null
    && typeof report === "object"
    && Array.isArray(report.recognized)
    && report.recognized.every(validRecognizedToken)
    && Array.isArray(report.unrecognized)
    && report.unrecognized.every(validUnrecognizedToken)
    && Array.isArray(report.nearest)
    && report.nearest.every((hint) => typeof hint?.example === "string");
}

function validRecognizedToken(item) {
  return item !== null
    && typeof item === "object"
    && typeof item.token === "string"
    && typeof item.target === "string";
}

function validUnrecognizedToken(item) {
  return item !== null
    && typeof item === "object"
    && typeof item.token === "string"
    && (item.suggestion === null || typeof item.suggestion === "string");
}

function validSchema(value) {
  return value !== null
    && typeof value === "object"
    && Array.isArray(value.node_tables)
    && Array.isArray(value.rel_tables);
}

function validQueryResult(value) {
  return value !== null
    && typeof value === "object"
    && Array.isArray(value.columns)
    && Array.isArray(value.rows);
}

function sameFold(left, right) {
  return typeof left === "string"
    && typeof right === "string"
    && asciiFold(left) === asciiFold(right);
}

function parsePosition(message) {
  const match = message.match(/\bposition\s+([1-9][0-9]*)\b/i);
  return match ? Number(match[1]) : null;
}

function errorMessage(error) {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function nextPaint() {
  return new Promise((resolve) => window.requestAnimationFrame(resolve));
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", boot, { once: true });
} else {
  boot();
}
