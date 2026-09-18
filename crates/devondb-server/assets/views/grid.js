const PAGE_SIZE = 500;
const GRID_BINDING = "grid";
const NUMERIC_TYPES = new Set(["Int64", "Float64"]);
const RESERVED_IDENTIFIERS = new Set([
  "and", "or", "not", "true", "false", "null", "as", "by", "asc", "desc",
  "out", "in", "both", "from", "to", "nodes", "expand", "filter", "project",
  "sort", "limit", "offset", "aggregate", "knn", "distance", "cosine", "l2",
  "count", "sum", "min", "max", "avg", "create", "insert", "copy", "update",
  "set", "delete", "where", "into", "values", "node", "rel", "table", "primary",
  "key",
]);
const FILTER_OPERATORS = Object.freeze({
  eq: "=",
  ne: "!=",
  lt: "<",
  le: "<=",
  gt: ">",
  ge: ">=",
});

export function registerGridView(registerView) {
  registerView("Grid", { render: renderGrid });
}

export function createGridRequest(schema, gridState = {}, changes = {}) {
  const tables = Array.isArray(schema?.node_tables) ? schema.node_tables : [];
  const requestedTable = changedValue(changes, "table", gridState.table);
  const table = findTable(tables, requestedTable) ?? tables[0];
  if (!table) {
    return null;
  }

  const tableChanged = typeof changes.table === "string"
    && !sameFold(changes.table, gridState.table);
  const sortValue = tableChanged ? null : changedValue(changes, "sort", gridState.sort);
  const filterValue = tableChanged ? [] : changedValue(changes, "filters", gridState.filters);
  const offsetValue = tableChanged ? 0 : changedValue(changes, "offset", gridState.offset);
  const sort = normalizeSort(sortValue, table);
  const filters = normalizeFilters(filterValue, table);
  const offset = nonnegativeInteger(offsetValue) ?? 0;

  return {
    table: table.name,
    sort,
    filters,
    offset,
    plan: buildGridPlan(table, sort, filters, offset),
  };
}

function buildGridPlan(table, sort, filters, offset) {
  let plan = { op: "ScanNodes", table: table.name, binding: GRID_BINDING };
  for (const filter of filters) {
    plan = {
      op: "Filter",
      predicate: {
        [filter.operator]: [
          { col: columnReference(filter.column) },
          { lit: filter.value },
        ],
      },
      input: plan,
    };
  }
  if (sort) {
    plan = {
      op: "Sort",
      keys: [{ expr: { col: columnReference(sort.column) }, order: sort.order }],
      input: plan,
    };
  }
  const limit = { op: "Limit", count: PAGE_SIZE, input: plan };
  if (offset > 0) {
    limit.offset = offset;
  }
  return { v: 0, plan: limit };
}

function renderGrid(container, model) {
  container.replaceChildren();
  const schema = model.schema;
  if (!validSchema(schema)) {
    const text = model.schemaLoading ? "Loading classes…" : "The schema is unavailable.";
    container.append(message(text, model.schemaLoading ? "loading-state" : "empty-state"));
    return;
  }
  if (schema.node_tables.length === 0) {
    container.append(message("Create a node table to use the grid."));
    return;
  }

  const table = findTable(schema.node_tables, model.grid?.table) ?? schema.node_tables[0];
  const classSummary = classForTable(schema, table.name);
  container.append(classPicker(schema, table, model));

  const panel = document.createElement("section");
  panel.id = "grid-table-panel";
  panel.className = "grid-table-panel";
  panel.setAttribute("role", "tabpanel");
  panel.setAttribute("aria-label", `${classDisplay(classSummary, table)} grid`);
  container.append(panel);

  if (model.grid?.resultTable !== table.name || model.result?.kind !== "query") {
    renderUnloaded(panel, table, model);
    return;
  }
  renderGridResult(panel, table, classSummary, model);
}

function classPicker(schema, activeTable, model) {
  const picker = document.createElement("section");
  picker.className = "grid-class-picker";
  picker.append(sectionLabel("classes"));

  const tabs = document.createElement("div");
  tabs.className = "grid-class-tabs";
  tabs.setAttribute("role", "tablist");
  tabs.setAttribute("aria-label", "Node classes");
  for (const table of schema.node_tables) {
    tabs.append(classChip(schema, table, activeTable, model));
  }
  installClassNavigation(tabs);
  picker.append(tabs);
  return picker;
}

function classChip(schema, table, activeTable, model) {
  const summary = classForTable(schema, table.name);
  const selected = sameFold(table.name, activeTable.name);
  const button = document.createElement("button");
  button.className = "grid-class-chip";
  button.type = "button";
  button.setAttribute("role", "tab");
  button.setAttribute("aria-selected", String(selected));
  button.setAttribute("aria-controls", "grid-table-panel");
  button.tabIndex = selected ? 0 : -1;
  button.disabled = model.gridBusy === true;
  button.dataset.gridClassTab = table.name;

  const color = classColor(summary);
  if (color) {
    const swatch = document.createElement("span");
    swatch.className = "grid-class-color";
    swatch.style.backgroundColor = color;
    swatch.setAttribute("aria-hidden", "true");
    button.append(swatch);
  }
  button.append(document.createTextNode(classDisplay(summary, table)));
  if (typeof summary?.description === "string") {
    button.title = summary.description;
  }
  button.addEventListener("click", () => {
    if (!selected) {
      runGrid(model, { table: table.name });
    }
  });
  return button;
}

function installClassNavigation(tablist) {
  const tabs = Array.from(tablist.querySelectorAll("[data-grid-class-tab]"));
  tabs.forEach((tab, index) => {
    tab.addEventListener("keydown", (event) => {
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
      }
    });
  });
}

function renderUnloaded(container, table, model) {
  if (model.gridBusy) {
    container.append(message("Explaining and running the class plan…", "loading-state"));
    return;
  }
  const state = document.createElement("div");
  state.className = "grid-load-state";
  const text = document.createElement("p");
  text.textContent = `Load all ${table.name} rows through DevonPlan.`;
  const button = actionButton("Load class", () => runGrid(model, { table: table.name }));
  state.append(text, button);
  container.append(state);
}

function renderGridResult(container, table, classSummary, model) {
  const result = model.result.data;
  if (!validQueryResult(result)) {
    container.append(message("The server returned a malformed grid result.", "result-error"));
    return;
  }
  const columns = orderedColumns(table, classSummary);
  const indices = resultIndices(result.columns, columns);
  if (indices.some((index) => index < 0)) {
    container.append(message("The grid result did not match the selected class.", "result-error"));
    return;
  }

  container.append(gridSummary(result, model.grid));
  const form = filterForm(table, columns, indices, result.rows, model);
  container.append(form);
  container.append(pagingControls(result, model));
}

function gridSummary(result, grid) {
  const summary = document.createElement("div");
  summary.className = "result-summary grid-result-summary";
  const count = totalRows(result);
  const offset = nonnegativeInteger(grid?.offset) ?? 0;
  const range = count === 0 ? `offset ${offset}` : `rows ${offset + 1}–${offset + count}`;
  const text = document.createElement("span");
  text.textContent = `${count} ${count === 1 ? "row" : "rows"} on this page · ${range}`;
  summary.append(text);
  if (result.truncated === true) {
    const warning = document.createElement("span");
    warning.className = "warning-badge";
    warning.textContent = "truncated by server";
    summary.append(warning);
  }
  return summary;
}

function filterForm(table, columns, indices, rows, model) {
  const form = document.createElement("form");
  form.className = "grid-filter-form";
  form.setAttribute("aria-label", `Filter ${table.name} rows`);

  const writeConfirmation = createWriteConfirmation(model);

  const scroll = document.createElement("div");
  scroll.className = "table-scroll grid-scroll";
  const tableElement = document.createElement("table");
  tableElement.className = "result-table grid-table";
  tableElement.append(
    gridHead(table, columns, model),
    gridBody(table, columns, indices, rows, model, writeConfirmation),
  );
  scroll.append(tableElement);

  const actions = document.createElement("div");
  actions.className = "grid-filter-actions";
  const status = document.createElement("span");
  status.className = "grid-filter-status";
  status.setAttribute("role", "alert");
  status.setAttribute("aria-live", "polite");
  const buttons = document.createElement("div");
  buttons.className = "grid-filter-buttons";
  const clear = actionButton("Clear filters", () => runGrid(model, { filters: [], offset: 0 }));
  clear.classList.add("secondary");
  clear.disabled = model.gridBusy === true || (model.grid?.filters?.length ?? 0) === 0;
  const apply = actionButton("Apply filters");
  apply.type = "submit";
  apply.classList.add("primary");
  apply.disabled = model.gridBusy === true;
  buttons.append(clear, apply);
  actions.append(status, buttons);
  form.append(scroll, writeConfirmation.element, actions);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const parsed = readFilters(form, table);
    if (!parsed.ok) {
      status.textContent = parsed.error;
      return;
    }
    status.textContent = "";
    runGrid(model, { filters: parsed.filters, offset: 0 });
  });
  return form;
}

function gridHead(table, columns, model) {
  const head = document.createElement("thead");
  const labels = document.createElement("tr");
  labels.className = "grid-column-row";
  for (const column of columns) {
    labels.append(columnHeader(column, model));
  }
  labels.append(rowActionHeader());
  const filters = document.createElement("tr");
  filters.className = "grid-filter-row";
  for (const column of columns) {
    filters.append(filterCell(table, column, model));
  }
  filters.append(rowActionFilterCell());
  head.append(labels, filters);
  return head;
}

function rowActionHeader() {
  const header = document.createElement("th");
  header.scope = "col";
  header.textContent = "Actions";
  header.style.minWidth = "0";
  header.style.width = "1%";
  return header;
}

function rowActionFilterCell() {
  const cell = document.createElement("td");
  cell.className = "grid-filter-unavailable";
  cell.setAttribute("aria-hidden", "true");
  cell.style.minWidth = "0";
  cell.style.width = "1%";
  return cell;
}

function columnHeader(column, model) {
  const header = document.createElement("th");
  header.scope = "col";
  const direction = sameFold(model.grid?.sort?.column, column.name)
    ? model.grid.sort.order
    : null;
  header.setAttribute("aria-sort", direction === "asc"
    ? "ascending"
    : direction === "desc" ? "descending" : "none");

  const button = document.createElement("button");
  button.className = "grid-sort-button";
  button.type = "button";
  button.disabled = model.gridBusy === true;
  button.setAttribute("aria-label", sortLabel(column.name, direction));
  const name = document.createElement("span");
  name.textContent = column.name;
  button.append(name);
  if (column.primary_key === true) {
    const key = document.createElement("span");
    key.className = "grid-primary-key";
    key.textContent = "PK";
    button.append(key);
  }
  const mark = document.createElement("span");
  mark.className = "grid-sort-mark";
  mark.setAttribute("aria-hidden", "true");
  mark.textContent = direction === "asc" ? "↑" : direction === "desc" ? "↓" : "↕";
  button.append(mark);
  button.addEventListener("click", () => {
    runGrid(model, { sort: nextSort(column.name, direction), offset: 0 });
  });
  header.append(button);
  return header;
}

function filterCell(table, column, model) {
  const cell = document.createElement("td");
  const comparators = comparatorsFor(column.type);
  if (comparators.length === 0) {
    cell.className = "grid-filter-unavailable";
    cell.textContent = "not filterable";
    return cell;
  }
  const applied = (model.grid?.filters ?? []).find((filter) => sameFold(filter.column, column.name));
  const controls = document.createElement("div");
  controls.className = "grid-filter-controls";
  controls.dataset.gridFilterColumn = column.name;
  controls.dataset.gridFilterType = column.type;

  const operator = document.createElement("select");
  operator.dataset.gridFilterOperator = "true";
  operator.setAttribute("aria-label", `${table.name} ${column.name} filter comparator`);
  appendOption(operator, "", "No filter");
  for (const comparator of comparators) {
    appendOption(operator, comparator, FILTER_OPERATORS[comparator]);
  }
  operator.value = applied?.operator ?? "";

  const value = literalControl(column.type, applied?.value);
  value.dataset.gridFilterValue = "true";
  value.setAttribute("aria-label", `${table.name} ${column.name} filter value`);
  value.disabled = operator.value === "" || model.gridBusy === true;
  operator.disabled = model.gridBusy === true;
  operator.addEventListener("change", () => {
    value.disabled = operator.value === "";
  });
  controls.append(operator, value);
  cell.append(controls);
  return cell;
}

function gridBody(table, columns, indices, rows, model, writeConfirmation) {
  const body = document.createElement("tbody");
  for (const values of rows) {
    const row = document.createElement("tr");
    const orderedValues = columns.map((_, index) => (
      Array.isArray(values) ? values[indices[index]] : undefined
    ));
    const keyColumn = table.columns.find((column) => column.primary_key === true);
    const keyIndex = columns.findIndex((column) => sameFold(column.name, keyColumn?.name));
    const rowContext = { keyColumn, key: orderedValues[keyIndex] };
    columns.forEach((column, index) => {
      const value = Array.isArray(values) ? values[indices[index]] : undefined;
      row.append(valueCell(value, column, table, rowContext, model, writeConfirmation));
    });
    row.append(deleteCell(table, rowContext, row, model, writeConfirmation));
    body.append(row);
  }
  if (rows.length === 0) {
    const row = document.createElement("tr");
    const cell = document.createElement("td");
    cell.className = "grid-no-rows";
    cell.colSpan = Math.max(columns.length + 1, 1);
    cell.textContent = "No rows match this page and filter set.";
    row.append(cell);
    body.append(row);
  }
  return body;
}

function pagingControls(result, model) {
  const paging = document.createElement("div");
  paging.className = "grid-paging";
  const offset = nonnegativeInteger(model.grid?.offset) ?? 0;
  if (offset > 0) {
    const first = actionButton("First page", () => runGrid(model, { offset: 0 }));
    first.classList.add("secondary");
    first.disabled = model.gridBusy === true;
    paging.append(first);
  }
  if (totalRows(result) >= PAGE_SIZE || result.truncated === true) {
    const more = actionButton("Load more", () => runGrid(model, { offset: offset + PAGE_SIZE }));
    more.classList.add("secondary");
    more.disabled = model.gridBusy === true;
    more.setAttribute("aria-label", `Load rows after offset ${offset + PAGE_SIZE}`);
    paging.append(more);
  }
  return paging;
}

function readFilters(form, table) {
  const filters = [];
  const controls = form.querySelectorAll("[data-grid-filter-column]");
  for (const control of controls) {
    const operator = control.querySelector("[data-grid-filter-operator]")?.value;
    if (!operator) {
      continue;
    }
    const column = findColumn(table, control.dataset.gridFilterColumn);
    const input = control.querySelector("[data-grid-filter-value]");
    const literal = readLiteral(input, column?.type);
    if (!column || !comparatorsFor(column.type).includes(operator)) {
      return { ok: false, error: "Choose a type-matched filter comparator." };
    }
    if (!literal.ok) {
      return { ok: false, error: `${column.name}: ${literal.error}` };
    }
    filters.push({ column: column.name, operator, value: literal.value });
  }
  return { ok: true, filters };
}

function normalizeSort(sort, table) {
  const column = findColumn(table, sort?.column);
  if (!column || (sort.order !== "asc" && sort.order !== "desc")) {
    return null;
  }
  return { column: column.name, order: sort.order };
}

function normalizeFilters(filters, table) {
  if (!Array.isArray(filters)) {
    return [];
  }
  const byColumn = new Map();
  for (const filter of filters) {
    const column = findColumn(table, filter?.column);
    if (column && comparatorsFor(column.type).includes(filter.operator)) {
      byColumn.set(asciiFold(column.name), {
        column: column.name,
        operator: filter.operator,
        value: filter.value,
      });
    }
  }
  return table.columns
    .map((column) => byColumn.get(asciiFold(column.name)))
    .filter(Boolean);
}

function orderedColumns(table, classSummary) {
  const ordered = [];
  const seen = new Set();
  const append = (name) => {
    const column = findColumn(table, name);
    const key = asciiFold(column?.name ?? "");
    if (column && !seen.has(key)) {
      ordered.push(column);
      seen.add(key);
    }
  };
  for (const name of Array.isArray(classSummary?.summary) ? classSummary.summary : []) {
    append(name);
  }
  for (const column of table.columns) {
    append(column.name);
  }
  return ordered;
}

function resultIndices(resultColumns, columns) {
  return columns.map((column) => resultColumns.findIndex(
    (resultColumn) => sameFold(resultColumn, columnReference(column.name)),
  ));
}

function literalControl(type, value) {
  if (type === "Bool") {
    const select = document.createElement("select");
    appendOption(select, "true", "true");
    appendOption(select, "false", "false");
    select.value = value === false ? "false" : "true";
    return select;
  }
  const input = document.createElement("input");
  if (NUMERIC_TYPES.has(type)) {
    input.type = "number";
    input.step = type === "Int64" ? "1" : "any";
    input.value = typeof value === "number" ? String(value) : "";
  } else if (isLiteralTextType(type)) {
    input.type = "text";
    input.value = value === null ? "" : dmlLiteral(value, type);
  } else {
    input.type = "text";
    input.value = typeof value === "string" ? value : "";
  }
  return input;
}

function readLiteral(control, type) {
  if (!control) {
    return { ok: false, error: "Enter a filter value." };
  }
  if (type === "Bool") {
    return { ok: true, value: control.value === "true" };
  }
  if (NUMERIC_TYPES.has(type)) {
    if (control.value.trim() === "") {
      return { ok: false, error: `Enter a valid ${type} value.` };
    }
    const value = Number(control.value);
    if (!Number.isFinite(value) || (type === "Int64" && !Number.isSafeInteger(value))) {
      return { ok: false, error: `Enter a valid ${type} value.` };
    }
    return { ok: true, value };
  }
  return { ok: true, value: control.value };
}

function comparatorsFor(type) {
  if (NUMERIC_TYPES.has(type)) {
    return ["eq", "ne", "lt", "le", "gt", "ge"];
  }
  if (type === "String" || type === "Bool") {
    return ["eq", "ne"];
  }
  return [];
}

function nextSort(column, direction) {
  if (direction === "asc") {
    return { column, order: "desc" };
  }
  if (direction === "desc") {
    return null;
  }
  return { column, order: "asc" };
}

function sortLabel(column, direction) {
  if (direction === "asc") {
    return `Sort ${column} descending`;
  }
  if (direction === "desc") {
    return `Remove sort from ${column}`;
  }
  return `Sort ${column} ascending`;
}

function valueCell(value, column, table, rowContext, model, writeConfirmation) {
  const cell = document.createElement("td");
  renderCellValue(cell, value);
  const hint = readOnlyHint(column);
  if (hint) {
    cell.setAttribute("aria-readonly", "true");
    cell.setAttribute("aria-label", `${displayValue(value)}. ${hint}`);
    cell.title = hint;
    return cell;
  }
  cell.title = `Double-click to edit ${column.name}`;
  cell.addEventListener("dblclick", () => {
    if (model.gridBusy !== true) {
      openCellEditor(cell, value, column, table, rowContext, writeConfirmation);
    }
  });
  return cell;
}

function renderCellValue(cell, value) {
  cell.replaceChildren();
  cell.classList.toggle("null-value", value === null);
  if (value === null) {
    cell.textContent = "NULL";
    return;
  }
  cell.textContent = displayValue(value);
  cell.title = cell.textContent;
}

function readOnlyHint(column) {
  if (column.primary_key === true) {
    return "Primary-key cells are read-only because grid updates are addressed by the primary key.";
  }
  return null;
}

function openCellEditor(cell, value, column, table, rowContext, writeConfirmation) {
  const editor = createCellEditor(column, value);
  const restore = () => {
    renderCellValue(cell, value);
    cell.title = `Double-click to edit ${column.name}`;
  };
  writeConfirmation.open(restore, editor.setBusy);
  cell.classList.remove("null-value");
  cell.replaceChildren(editor.element);

  const preview = async () => {
    const literal = editor.read();
    if (!literal.ok) {
      editor.showError(literal.error);
      return;
    }
    try {
      const statement = composeUpdate(table, column, literal.value, rowContext);
      editor.showError("");
      await writeConfirmation.preview(statement);
    } catch (error) {
      editor.showError(errorMessage(error));
    }
  };
  editor.preview.addEventListener("click", preview);
  editor.cancel.addEventListener("click", writeConfirmation.cancel);
  editor.element.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      writeConfirmation.cancel();
    } else if (event.key === "Enter") {
      event.preventDefault();
      preview();
    }
  });
  editor.focus();
}

function createCellEditor(column, value) {
  const element = document.createElement("div");
  const controls = document.createElement("div");
  controls.className = "grid-filter-controls";
  const control = literalControl(column.type, value);
  control.setAttribute("aria-label", `${column.name} ${column.type} value`);
  appendQuotedControl(controls, control, column.type);

  const nullLabel = document.createElement("label");
  const nullControl = document.createElement("input");
  nullControl.type = "checkbox";
  nullControl.checked = value === null;
  nullControl.setAttribute("aria-label", `Set ${column.name} to NULL`);
  nullLabel.append(nullControl, document.createTextNode(" NULL"));
  controls.append(nullLabel);

  const error = document.createElement("span");
  error.className = "grid-filter-status";
  error.setAttribute("role", "alert");
  const buttons = document.createElement("div");
  buttons.className = "grid-filter-buttons";
  const cancel = actionButton("Cancel");
  cancel.classList.add("secondary");
  const preview = actionButton("Preview change");
  preview.classList.add("primary");
  buttons.append(cancel, preview);
  element.append(controls, error, buttons);

  const syncNull = () => {
    control.disabled = nullControl.checked;
  };
  syncNull();
  nullControl.addEventListener("change", syncNull);
  return {
    element,
    preview,
    cancel,
    read: () => readEditedLiteral(control, column.type, nullControl.checked),
    showError: (text) => { error.textContent = text; },
    setBusy: (busy) => {
      control.disabled = busy || nullControl.checked;
      nullControl.disabled = busy;
      preview.disabled = busy;
    },
    focus: () => {
      (nullControl.checked ? nullControl : control).focus();
      if (!nullControl.checked && typeof control.select === "function") {
        control.select();
      }
    },
  };
}

function appendQuotedControl(container, control, type) {
  if (type !== "String") {
    container.append(control);
    return;
  }
  const quoted = document.createElement("span");
  quoted.title = "String values are composed as quoted DevonPlan literals.";
  quoted.append(document.createTextNode("\""), control, document.createTextNode("\""));
  container.append(quoted);
}

function deleteCell(table, rowContext, row, model, writeConfirmation) {
  const cell = document.createElement("td");
  cell.style.minWidth = "0";
  cell.style.width = "1%";
  const button = actionButton("Delete");
  button.classList.add("secondary");
  button.hidden = true;
  button.disabled = model.gridBusy === true;
  button.setAttribute(
    "aria-label",
    `Delete ${table.name} row ${displayValue(rowContext.key)}`,
  );
  button.addEventListener("click", async () => {
    writeConfirmation.open(() => {}, () => {});
    try {
      await writeConfirmation.preview(composeDelete(table, rowContext));
    } catch (error) {
      writeConfirmation.fail(errorMessage(error));
    }
  });
  installRowDeleteReveal(row, button);
  cell.append(button);
  return cell;
}

function installRowDeleteReveal(row, button) {
  row.tabIndex = 0;
  const show = () => { button.hidden = false; };
  const hide = () => {
    if (!row.contains(document.activeElement)) {
      button.hidden = true;
    }
  };
  row.addEventListener("mouseenter", show);
  row.addEventListener("mouseleave", hide);
  row.addEventListener("focusin", show);
  row.addEventListener("focusout", (event) => {
    if (!row.contains(event.relatedTarget)) {
      button.hidden = true;
    }
  });
}

function createWriteConfirmation(model) {
  const element = document.createElement("section");
  element.className = "canonical-block";
  element.hidden = true;
  element.tabIndex = -1;
  element.setAttribute("aria-label", "Confirm canonical write statement");
  element.setAttribute("aria-keyshortcuts", "Enter Escape");
  const label = sectionLabel("canonical statement");
  const canonical = document.createElement("pre");
  canonical.className = "canonical-text";
  const status = document.createElement("div");
  status.className = "grid-filter-status";
  status.setAttribute("role", "alert");
  status.setAttribute("aria-live", "polite");
  const buttons = document.createElement("div");
  buttons.className = "grid-filter-buttons";
  const cancel = actionButton("Cancel");
  cancel.classList.add("secondary");
  const apply = actionButton("Apply");
  apply.classList.add("primary");
  apply.disabled = true;
  buttons.append(cancel, apply);
  element.append(label, canonical, status, buttons);

  let active = null;
  let requestId = 0;
  const close = () => {
    requestId += 1;
    const previous = active;
    active = null;
    element.hidden = true;
    canonical.textContent = "";
    status.textContent = "";
    apply.disabled = true;
    cancel.disabled = false;
    previous?.setBusy(false);
    previous?.cancel();
  };
  const open = (cancelAction, setBusy) => {
    close();
    active = { cancel: cancelAction, setBusy, canonical: null, busy: false };
  };
  const fail = (messageText) => {
    element.hidden = false;
    canonical.textContent = "";
    status.textContent = messageText;
    apply.disabled = true;
    element.focus();
  };
  const preview = (statement) => previewWrite(statement, active, {
    element, canonical, status, apply, nextRequest: () => ++requestId,
    currentRequest: () => requestId,
  });
  const land = () => landWrite(active, model, { status, apply, cancel });

  cancel.addEventListener("click", close);
  apply.addEventListener("click", land);
  element.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      close();
    } else if (event.key === "Enter" && event.target !== cancel) {
      event.preventDefault();
      land();
    }
  });
  return { element, open, preview, cancel: close, fail };
}

async function previewWrite(statement, active, view) {
  if (!active || active.busy) {
    return;
  }
  const token = view.nextRequest();
  active.busy = true;
  active.setBusy(true);
  view.element.hidden = false;
  view.canonical.textContent = "";
  view.status.textContent = "Asking the engine for its canonical statement…";
  view.apply.disabled = true;
  view.element.focus();
  try {
    const response = await postJson("/api/explain", { text: statement });
    if (response?.kind !== "statement" || typeof response.canonical !== "string") {
      throw new Error("The engine returned a malformed statement explanation.");
    }
    if (token !== view.currentRequest()) {
      return;
    }
    active.canonical = response.canonical;
    view.canonical.textContent = response.canonical;
    view.status.textContent = "Press Enter to apply. Esc cancels.";
    view.apply.disabled = false;
    view.element.focus();
  } catch (error) {
    if (token === view.currentRequest()) {
      view.status.textContent = errorMessage(error);
    }
  } finally {
    if (token === view.currentRequest()) {
      active.busy = false;
      active.setBusy(false);
    }
  }
}

async function landWrite(active, model, controls) {
  if (!active?.canonical || active.busy) {
    return;
  }
  active.busy = true;
  active.setBusy(true);
  controls.apply.disabled = true;
  controls.cancel.disabled = true;
  controls.status.textContent = "Applying canonical statement…";
  try {
    const response = await postJson("/api/statement", { text: active.canonical });
    if (response?.ok !== true) {
      throw new Error("The engine returned a malformed statement result.");
    }
    controls.status.textContent = "Applied. Refreshing the current page…";
    runGrid(model, {});
  } catch (error) {
    controls.status.textContent = errorMessage(error);
    active.busy = false;
    active.setBusy(false);
    controls.apply.disabled = false;
    controls.cancel.disabled = false;
  }
}

function composeUpdate(table, column, value, rowContext) {
  const keyColumn = requiredKeyColumn(rowContext);
  return `update ${dmlIdentifier(table.name)} set ${dmlIdentifier(column.name)} = ${dmlLiteral(value, column.type)} where ${dmlIdentifier(keyColumn.name)} = ${dmlLiteral(rowContext.key, keyColumn.type)}`;
}

function composeDelete(table, rowContext) {
  const keyColumn = requiredKeyColumn(rowContext);
  return `delete from ${dmlIdentifier(table.name)} where ${dmlIdentifier(keyColumn.name)} = ${dmlLiteral(rowContext.key, keyColumn.type)}`;
}

function requiredKeyColumn(rowContext) {
  if (!rowContext.keyColumn || rowContext.key === undefined) {
    throw new Error("The grid row does not carry its primary-key value.");
  }
  return rowContext.keyColumn;
}

function dmlIdentifier(identifier) {
  const text = String(identifier);
  if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(text) && !RESERVED_IDENTIFIERS.has(text)) {
    return text;
  }
  if (text.length === 0) {
    throw new Error("The grid cannot compose an empty identifier.");
  }
  return `\`${escapeText(text, "\`")}\``;
}

function dmlLiteral(value, type) {
  if (value === null) {
    return "null";
  }
  if (isLiteralTextType(type) && typeof value === "string") {
    return value;
  }
  if (type === "Bool" && typeof value === "boolean") {
    return value ? "true" : "false";
  }
  if (type === "Int64" && Number.isSafeInteger(value)) {
    return String(value);
  }
  if (type === "Float64" && typeof value === "number" && Number.isFinite(value)) {
    return float64Literal(value);
  }
  if (type === "String" && typeof value === "string") {
    return `"${escapeText(value, "\"")}"`;
  }
  if (String(type).startsWith("Vector") && Array.isArray(value)
      && value.every((element) => typeof element === "number" && Number.isFinite(element))) {
    const elements = value.map((element) => Object.is(element, -0) ? "-0" : String(element));
    return `[${elements.join(", ")}]`;
  }
  const point = value?.geo;
  if (type === "GeoPoint" && point
      && typeof point.lat_deg === "number" && Number.isFinite(point.lat_deg)
      && typeof point.lng_deg === "number" && Number.isFinite(point.lng_deg)) {
    return `geo(${float64Literal(point.lat_deg)}, ${float64Literal(point.lng_deg)})`;
  }
  throw new Error(`The grid cannot compose a ${type} literal from this value.`);
}

function float64Literal(value) {
  if (Object.is(value, -0)) {
    return "-0.0";
  }
  const text = String(value);
  return /[.eE]/.test(text) ? text : `${text}.0`;
}

function isLiteralTextType(type) {
  return type === "GeoPoint" || String(type).startsWith("Vector");
}

function escapeText(value, delimiter) {
  let output = "";
  for (const character of value) {
    if (character === "\\") {
      output += "\\\\";
    } else if (character === "\n") {
      output += "\\n";
    } else if (character === "\r") {
      output += "\\r";
    } else if (character === "\t") {
      output += "\\t";
    } else if (character === delimiter) {
      output += `\\${character}`;
    } else if (delimiter === "\"" && character.codePointAt(0) <= 0x1f) {
      output += `\\u{${character.codePointAt(0).toString(16)}}`;
    } else {
      output += character;
    }
  }
  return output;
}

function readEditedLiteral(control, type, isNull) {
  if (isNull) {
    return { ok: true, value: null };
  }
  if (isLiteralTextType(type)) {
    return control.value.trim() === ""
      ? { ok: false, error: `Enter a non-empty ${type} literal.` }
      : { ok: true, value: control.value };
  }
  return readLiteral(control, type);
}

async function postJson(path, body) {
  let response;
  try {
    response = await fetch(path, {
      method: "POST",
      headers: { Accept: "application/json", "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch (error) {
    throw new Error(`Request to ${path} failed: ${errorMessage(error)}`);
  }
  const text = await response.text();
  let payload = null;
  try {
    payload = text.length === 0 ? null : JSON.parse(text);
  } catch (error) {
    throw new Error(`Invalid JSON from ${path}: ${errorMessage(error)}`);
  }
  if (!response.ok) {
    throw new Error(typeof payload?.error === "string"
      ? payload.error
      : `${response.status} ${response.statusText}`);
  }
  return payload;
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function displayValue(value) {
  if (value === undefined) {
    return "";
  }
  return typeof value === "object" ? JSON.stringify(value) : String(value);
}

function classForTable(schema, tableName) {
  const classes = Array.isArray(schema.classes?.node_classes)
    ? schema.classes.node_classes
    : [];
  return classes.find((item) => sameFold(item?.table, tableName)) ?? null;
}

function classDisplay(summary, table) {
  return typeof summary?.display === "string" && summary.display.length > 0
    ? summary.display
    : table.name;
}

function classColor(summary) {
  return typeof summary?.color === "string" && summary.color.length > 0
    ? summary.color
    : null;
}

function findTable(tables, name) {
  return tables.find((table) => sameFold(table?.name, name));
}

function findColumn(table, name) {
  return (table?.columns ?? []).find((column) => sameFold(column?.name, name));
}

function changedValue(changes, key, fallback) {
  return Object.hasOwn(changes, key) ? changes[key] : fallback;
}

function columnReference(column) {
  return `${GRID_BINDING}.${column}`;
}

function runGrid(model, changes) {
  const request = createGridRequest(model.schema, model.grid, changes);
  if (request) {
    model.actions?.runGrid?.(request);
  }
}

function actionButton(text, action) {
  const button = document.createElement("button");
  button.className = "button";
  button.type = "button";
  button.textContent = text;
  if (action) {
    button.addEventListener("click", action);
  }
  return button;
}

function appendOption(select, value, label) {
  const option = document.createElement("option");
  option.value = value;
  option.textContent = label;
  select.append(option);
}

function sectionLabel(text) {
  const label = document.createElement("span");
  label.className = "section-label";
  label.textContent = text;
  return label;
}

function totalRows(result) {
  const declared = Number.isSafeInteger(result.row_count) && result.row_count >= 0
    ? result.row_count
    : result.rows.length;
  return Math.max(declared, result.rows.length);
}

function nonnegativeInteger(value) {
  const number = Number(value);
  return Number.isSafeInteger(number) && number >= 0 ? number : null;
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

function message(text, className = "empty-state") {
  const paragraph = document.createElement("p");
  paragraph.className = className;
  paragraph.textContent = text;
  return paragraph;
}

function sameFold(left, right) {
  return typeof left === "string"
    && typeof right === "string"
    && asciiFold(left) === asciiFold(right);
}

function asciiFold(value) {
  return String(value).replace(/[A-Z]/g, (character) => (
    String.fromCharCode(character.charCodeAt(0) + 32)
  ));
}
