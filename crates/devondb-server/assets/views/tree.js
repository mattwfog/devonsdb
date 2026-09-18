const INPUT_FIELD = "input";
const NUMERIC_TYPES = new Set(["Int64", "Float64"]);
const FILTER_OPERATORS = Object.freeze({
  eq: "=",
  ne: "!=",
  lt: "<",
  le: "<=",
  gt: ">",
  ge: ">=",
});
const STAGE_LABELS = Object.freeze({
  Filter: "Filter rows",
  Expand: "Expand relationship",
  Project: "Project columns",
  Sort: "Sort rows",
  Limit: "Limit rows",
  Aggregate: "Aggregate rows",
});

export function renderQueryTree(canonicalJson, schema, options = {}) {
  if (!isPlan(canonicalJson)) {
    return null;
  }

  const section = document.createElement("section");
  section.className = "query-tree";
  section.append(sectionLabel("interactive query tree"));

  const hint = document.createElement("p");
  hint.className = "tree-hint";
  hint.textContent = "Choose a node to edit it. Every choice is scoped to the schema available there.";
  section.append(hint);

  const chain = operatorChain(canonicalJson.plan);
  const contexts = pipelineContexts(chain, schema);
  const list = document.createElement("ol");
  list.className = "operator-chain";
  list.setAttribute("role", "tree");
  list.setAttribute("aria-label", "DevonPlan operator tree");

  chain.forEach((operator, index) => {
    list.append(operatorItem(canonicalJson, chain, contexts, index, options));
  });
  section.append(list);
  installRovingTabindex(list);
  return section;
}

function operatorItem(envelope, chain, contexts, index, options) {
  const operator = chain[index];
  const nodeId = `node-${index}`;
  const item = document.createElement("li");
  item.className = "operator-chain-item";
  item.setAttribute("role", "treeitem");
  item.setAttribute("aria-level", String(index + 1));

  const card = document.createElement("article");
  card.className = "tree-operator-card";
  if (options.error?.nodeId === nodeId) {
    card.classList.add("has-error");
  }

  const editor = operatorEditor(operator, contexts[index].before, chain.length > 1, (next) => {
    const candidate = index === 0
      ? rebuildEnvelope(envelope, [next])
      : replaceOperator(envelope, chain, index, next);
    options.onMutate?.(candidate, nodeId);
  });
  card.append(operatorHeader(operator, index, editor, envelope, chain, options, nodeId));
  card.append(operatorBody(operator));
  if (editor) {
    editor.hidden = true;
    card.append(editor);
  }
  appendMutationStatus(card, options, nodeId);
  item.append(card);

  const isTail = index === chain.length - 1;
  const stages = availableStages(contexts[index].after, isTail);
  if (stages.length > 0) {
    item.append(addStageControl(envelope, chain, index, contexts[index].after, stages, options));
  }
  return item;
}

function operatorHeader(operator, index, editor, envelope, chain, options, nodeId) {
  const header = document.createElement("header");
  header.className = "tree-operator-header";

  const toggle = document.createElement("button");
  toggle.className = "tree-node-toggle";
  toggle.type = "button";
  toggle.dataset.treeRover = "true";
  toggle.disabled = treeDisabled(options) || !editor;
  toggle.setAttribute("aria-expanded", "false");
  toggle.setAttribute("aria-label", `${editor ? "Edit" : "Inspect"} ${operatorTitle(operator)}`);
  toggle.append(stepNumber(index), document.createTextNode(operatorTitle(operator)));
  if (editor) {
    toggle.addEventListener("click", () => toggleEditor(toggle, editor));
  }
  header.append(toggle);

  if (index > 0) {
    const remove = document.createElement("button");
    remove.className = "tree-remove-button";
    remove.type = "button";
    remove.disabled = treeDisabled(options);
    remove.setAttribute("aria-label", `Remove ${operatorTitle(operator)}`);
    remove.textContent = "Remove";
    remove.addEventListener("click", () => {
      options.onMutate?.(removeOperator(envelope, chain, index), nodeId);
    });
    header.append(remove);
  }
  return header;
}

function operatorBody(operator) {
  const body = document.createElement("div");
  body.className = "tree-operator-body";
  const facts = operatorFacts(operator);
  if (facts.length > 0) {
    body.append(factList(facts));
  }
  for (const expression of operatorExpressions(operator)) {
    body.append(expressionBranch(expression.label, expression.value));
  }
  return body;
}

function operatorFacts(operator) {
  switch (operator.op) {
    case "ScanNodes":
      return [["table", operator.table], ["binding", operator.binding]];
    case "KnnScan":
      return [
        ["table", operator.table], ["vector", operator.column], ["k", operator.k],
        ["metric", operator.metric], ["mode", operator.mode ?? "exact"],
      ];
    case "Expand":
      return [
        ["relationship", operator.rel], ["direction", operator.direction],
        ["from", operator.from_binding], ["binding", operator.binding],
      ];
    case "Limit":
      return [["count", operator.count], ["offset", operator.offset ?? "not set"]];
    default:
      return [];
  }
}

function operatorExpressions(operator) {
  switch (operator.op) {
    case "Filter":
      return [{ label: "predicate", value: operator.predicate }];
    case "Project":
      return (operator.exprs ?? []).map((item) => ({
        label: `output ${item.as}`,
        value: item.expr,
      }));
    case "Sort":
      return (operator.keys ?? []).map((key) => ({
        label: `sort ${key.order}`,
        value: key.expr,
      }));
    case "Aggregate":
      return [
        ...(operator.aggs ?? []).map((item) => ({
          label: `${item.fn} as ${item.as}`,
          value: item.expr,
        })),
        ...(operator.group_by ?? []).map((value) => ({ label: "group by", value })),
      ];
    case "KnnScan":
      return [{ label: "query vector", value: { lit: operator.query } }];
    default:
      return [];
  }
}

function expressionBranch(label, expression) {
  const branch = document.createElement("div");
  branch.className = "expression-branch";
  const heading = document.createElement("span");
  heading.className = "expression-label";
  heading.textContent = label;
  branch.append(heading, expressionList(expression));
  return branch;
}

function expressionList(expression) {
  const list = document.createElement("ul");
  list.className = "expression-tree";
  const item = document.createElement("li");
  const description = document.createElement("span");
  description.className = "expression-node";
  description.textContent = expressionDescription(expression);
  item.append(description);
  for (const child of expressionChildren(expression)) {
    item.append(expressionList(child));
  }
  list.append(item);
  return list;
}

function expressionDescription(expression) {
  if (!isObject(expression)) {
    return compactValue(expression);
  }
  if (Object.hasOwn(expression, "col")) {
    return `column · ${expression.col}`;
  }
  if (Object.hasOwn(expression, "lit")) {
    return `value · ${compactValue(expression.lit)}`;
  }
  const key = Object.keys(expression)[0];
  if (key === "distance") {
    return `distance · ${expression.distance?.metric ?? "unknown"}`;
  }
  return FILTER_OPERATORS[key] ?? key ?? "expression";
}

function expressionChildren(expression) {
  if (!isObject(expression)) {
    return [];
  }
  const key = Object.keys(expression)[0];
  const value = expression[key];
  if (Array.isArray(value) && key !== "lit") {
    return value;
  }
  if (key === "not") {
    return [value];
  }
  if (key === "distance" && isObject(value)) {
    return [value.left, value.right];
  }
  return [];
}

function addStageControl(envelope, chain, index, context, stages, options) {
  const control = document.createElement("div");
  control.className = "tree-continuation";
  const toggle = document.createElement("button");
  toggle.className = "tree-add-button";
  toggle.type = "button";
  toggle.dataset.treeRover = "true";
  toggle.disabled = treeDisabled(options);
  toggle.setAttribute("aria-expanded", "false");
  toggle.textContent = `Add after ${operatorTitle(chain[index])}`;

  const panel = document.createElement("div");
  panel.className = "tree-add-panel";
  panel.hidden = true;
  const stageSelect = selectControl(stages.map((stage) => [stage, STAGE_LABELS[stage]]));
  panel.append(controlField("Continuation", stageSelect));

  const formHost = document.createElement("div");
  panel.append(formHost);
  const renderForm = () => {
    formHost.replaceChildren();
    const form = stageForm(stageSelect.value, context, null, "Add stage", (stage) => {
      const candidate = insertOperator(envelope, chain, index, stage);
      options.onMutate?.(candidate, `node-${index}`);
    });
    if (form) {
      formHost.append(form);
    }
  };
  stageSelect.addEventListener("change", renderForm);
  toggle.addEventListener("click", () => toggleEditor(toggle, panel));
  renderForm();
  control.append(toggle, panel);
  return control;
}

function operatorEditor(operator, context, hasDownstream, onSubmit) {
  if (operator.op === "ScanNodes" || operator.op === "KnnScan") {
    return sourceForm(operator, context.schema, hasDownstream, onSubmit);
  }
  return stageForm(operator.op, context, operator, "Apply changes", onSubmit);
}

function sourceForm(operator, schema, hasDownstream, onSubmit) {
  if (!validSchema(schema) || schema.node_tables.length === 0) {
    return null;
  }
  const form = formElement("Edit source");
  const table = selectControl(schema.node_tables.map((item) => [item.name, item.name]));
  table.value = findByFold(schema.node_tables, operator.table)?.name ?? schema.node_tables[0].name;
  const binding = textControl(operator.binding ?? bindingForTable(table.value));
  table.addEventListener("change", () => {
    binding.value = bindingForTable(table.value);
  });
  form.body.append(controlField("Node table", table), controlField("Binding", binding));
  if (hasDownstream) {
    form.body.append(formHint("Changing the source starts a fresh, valid pipeline."));
  }
  finishForm(form, "Use source", () => {
    const bindingError = invalidBinding(binding.value);
    if (bindingError) {
      return formFailure(form, bindingError);
    }
    onSubmit({ op: "ScanNodes", table: table.value, binding: binding.value });
    return true;
  });
  return form.element;
}

function stageForm(stage, context, current, submitLabel, onSubmit) {
  switch (stage) {
    case "Filter":
      return filterForm(context, current, submitLabel, onSubmit);
    case "Expand":
      return expandForm(context, current, submitLabel, onSubmit);
    case "Project":
      return projectForm(context, current, submitLabel, onSubmit);
    case "Sort":
      return sortForm(context, current, submitLabel, onSubmit);
    case "Limit":
      return limitForm(current, submitLabel, onSubmit);
    case "Aggregate":
      return aggregateForm(context, current, submitLabel, onSubmit);
    default:
      return null;
  }
}

function filterForm(context, current, submitLabel, onSubmit) {
  const columns = filterColumns(context.columns);
  if (columns.length === 0) {
    return unavailableForm("No scalar columns are in scope for a filter.");
  }
  const form = formElement("Filter choices");
  const parsed = simplePredicate(current?.predicate);
  const column = columnSelect(columns, parsed?.reference);
  const comparator = document.createElement("select");
  const valueHost = document.createElement("div");
  let valueInput;

  const refresh = () => {
    const selected = choiceByRef(columns, column.value);
    replaceOptions(comparator, comparatorsFor(selected?.type));
    if (parsed && comparator.querySelector(`option[value="${parsed.operator}"]`)) {
      comparator.value = parsed.operator;
    }
    valueHost.replaceChildren();
    valueInput = literalControl(selected?.type, parsed?.literal);
    valueHost.append(controlField("Value", valueInput));
  };
  column.addEventListener("change", refresh);
  form.body.append(controlField("Column", column), controlField("Comparator", comparator), valueHost);
  refresh();
  finishForm(form, submitLabel, () => {
    const choice = choiceByRef(columns, column.value);
    const literal = readLiteral(valueInput, choice?.type);
    if (!literal.ok) {
      return formFailure(form, literal.error);
    }
    onSubmit({
      op: "Filter",
      predicate: { [comparator.value]: [{ col: choice.ref }, { lit: literal.value }] },
    });
    return true;
  });
  return form.element;
}

function expandForm(context, current, submitLabel, onSubmit) {
  const choices = expandChoices(context);
  if (choices.length === 0) {
    return unavailableForm("No relationship is incident to an in-scope binding.");
  }
  const form = formElement("Relationship choices");
  const traversal = selectControl(choices.map((choice, index) => [String(index), choice.label]));
  const currentIndex = choices.findIndex((choice) => current
    && sameFold(choice.rel.name, current.rel)
    && sameFold(choice.fromBinding, current.from_binding)
    && choice.direction === current.direction);
  traversal.value = String(currentIndex < 0 ? 0 : currentIndex);
  const selected = () => choices[Number(traversal.value)] ?? choices[0];
  const binding = textControl(current?.binding ?? uniqueBinding(selected().target, context.nodes));
  if (!current) {
    traversal.addEventListener("change", () => {
      binding.value = uniqueBinding(selected().target, context.nodes);
    });
  }
  form.body.append(
    controlField("Traversal", traversal),
    controlField("New binding", binding),
    formHint("Only relationships touching the selected binding are listed."),
  );
  finishForm(form, submitLabel, () => {
    const bindingError = invalidBinding(binding.value);
    if (bindingError) {
      return formFailure(form, bindingError);
    }
    if (context.nodes.some((node) => sameFold(node.binding, binding.value))) {
      return formFailure(form, `Binding ${binding.value} is already in scope.`);
    }
    const choice = selected();
    onSubmit({
      op: "Expand",
      rel: choice.rel.name,
      direction: choice.direction,
      from_binding: choice.fromBinding,
      binding: binding.value,
    });
    return true;
  });
  return form.element;
}

function projectForm(context, current, submitLabel, onSubmit) {
  if (context.columns.length === 0) {
    return unavailableForm("No schema columns are in scope for a projection.");
  }
  const form = formElement("Projection choices");
  const selected = new Set((current?.exprs ?? [])
    .map((item) => item.expr?.col)
    .filter((value) => typeof value === "string")
    .map(asciiFold));
  const controls = choiceCheckboxes(context.columns, selected, !current);
  form.body.append(controls.fieldset);
  finishForm(form, submitLabel, () => {
    const choices = checkedChoices(controls.entries);
    if (choices.length === 0) {
      return formFailure(form, "Select at least one projected column.");
    }
    const aliases = projectionAliases(choices, current?.exprs ?? []);
    onSubmit({
      op: "Project",
      exprs: choices.map((choice, index) => ({ expr: { col: choice.ref }, as: aliases[index] })),
    });
    return true;
  });
  return form.element;
}

function sortForm(context, current, submitLabel, onSubmit) {
  if (context.columns.length === 0) {
    return unavailableForm("No schema columns are in scope for sorting.");
  }
  const form = formElement("Sort choices");
  const currentKeys = new Map((current?.keys ?? [])
    .filter((key) => typeof key.expr?.col === "string")
    .map((key) => [asciiFold(key.expr.col), key.order]));
  const fieldset = document.createElement("fieldset");
  fieldset.className = "tree-choice-list";
  fieldset.append(legend("Columns and direction"));
  const entries = context.columns.map((choice, index) => {
    const checkbox = checkboxControl(currentKeys.has(asciiFold(choice.ref)) || (!current && index === 0));
    const order = selectControl([["asc", "ascending"], ["desc", "descending"]]);
    order.value = currentKeys.get(asciiFold(choice.ref)) ?? "asc";
    const row = choiceRow(choice, checkbox);
    row.append(order);
    fieldset.append(row);
    return { choice, checkbox, order };
  });
  form.body.append(fieldset);
  finishForm(form, submitLabel, () => {
    const keys = entries.filter((entry) => entry.checkbox.checked).map((entry) => ({
      expr: { col: entry.choice.ref },
      order: entry.order.value,
    }));
    if (keys.length === 0) {
      return formFailure(form, "Select at least one sort column.");
    }
    onSubmit({ op: "Sort", keys });
    return true;
  });
  return form.element;
}

function limitForm(current, submitLabel, onSubmit) {
  const form = formElement("Limit choices");
  const count = numberControl(current?.count ?? 100, "1", "0");
  const hasOffset = Object.hasOwn(current ?? {}, "offset");
  const includeOffset = checkboxControl(hasOffset);
  const offset = numberControl(current?.offset ?? 0, "1", "0");
  offset.disabled = !includeOffset.checked;
  includeOffset.addEventListener("change", () => {
    offset.disabled = !includeOffset.checked;
  });
  form.body.append(
    controlField("Maximum rows", count),
    checkboxField("Set an offset", includeOffset),
    controlField("Rows to skip", offset),
  );
  finishForm(form, submitLabel, () => {
    const countValue = nonnegativeInteger(count.value);
    const offsetValue = nonnegativeInteger(offset.value);
    if (countValue === null || (includeOffset.checked && offsetValue === null)) {
      return formFailure(form, "Count and offset must be non-negative whole numbers.");
    }
    const stage = { op: "Limit", count: countValue };
    if (includeOffset.checked) {
      stage.offset = offsetValue;
    }
    onSubmit(stage);
    return true;
  });
  return form.element;
}

function aggregateForm(context, current, submitLabel, onSubmit) {
  if (context.columns.length === 0) {
    return unavailableForm("No schema columns are in scope for an aggregate.");
  }
  const form = formElement("Aggregate choices");
  const aggregate = current?.aggs?.[0];
  const fn = selectControl([
    ["count", "count"], ["sum", "sum"], ["min", "min"], ["max", "max"], ["avg", "avg"],
  ]);
  fn.value = aggregate?.fn ?? "count";
  const operand = document.createElement("select");
  const alias = textControl(aggregate?.as ?? "");
  const refreshOperands = () => {
    const choices = aggregateColumns(context.columns, fn.value);
    const previous = operand.value || aggregate?.expr?.col;
    replaceOptions(operand, choices.map((choice) => [choice.ref, choiceLabel(choice)]));
    const previousChoice = choiceByRef(choices, previous);
    if (previousChoice) {
      operand.value = previousChoice.ref;
    }
    if (!alias.value || !current) {
      const choice = choiceByRef(choices, operand.value);
      alias.value = `${fn.value}_${choice?.name ?? "value"}`;
    }
  };
  fn.addEventListener("change", refreshOperands);
  operand.addEventListener("change", () => {
    const choice = choiceByRef(context.columns, operand.value);
    alias.value = `${fn.value}_${choice?.name ?? "value"}`;
  });
  const groups = new Set((current?.group_by ?? [])
    .map((expr) => expr?.col)
    .filter((value) => typeof value === "string")
    .map(asciiFold));
  const groupControls = choiceCheckboxes(context.columns, groups, false, "Group by (optional)");
  form.body.append(
    controlField("Function", fn),
    controlField("Operand", operand),
    controlField("Output name", alias),
    groupControls.fieldset,
  );
  refreshOperands();
  finishForm(form, submitLabel, () => {
    if (alias.value.length === 0) {
      return formFailure(form, "Output name cannot be empty.");
    }
    const choice = choiceByRef(context.columns, operand.value);
    if (!choice) {
      return formFailure(form, "Choose a type-legal operand.");
    }
    onSubmit({
      op: "Aggregate",
      group_by: checkedChoices(groupControls.entries).map((item) => ({ col: item.ref })),
      aggs: [{ fn: fn.value, expr: { col: choice.ref }, as: alias.value }],
    });
    return true;
  });
  return form.element;
}

function availableStages(context, isTail) {
  const stages = [];
  if (filterColumns(context.columns).length > 0) {
    stages.push("Filter");
  }
  if (expandChoices(context).length > 0) {
    stages.push("Expand");
  }
  if (isTail && context.columns.length > 0) {
    stages.push("Project");
  }
  if (context.columns.length > 0) {
    stages.push("Sort");
  }
  stages.push("Limit");
  if (isTail && context.columns.length > 0) {
    stages.push("Aggregate");
  }
  return stages;
}

function pipelineContexts(chain, schema) {
  let context = emptyContext(schema);
  return chain.map((operator) => {
    const before = cloneContext(context);
    context = applyOperator(context, operator);
    return { before, after: cloneContext(context) };
  });
}

function applyOperator(context, operator) {
  switch (operator.op) {
    case "ScanNodes":
      return sourceContext(context.schema, operator.table, operator.binding);
    case "KnnScan":
      return sourceContext(context.schema, operator.table, operator.table);
    case "Expand":
      return expandedContext(context, operator);
    case "Project":
      return projectedContext(context, operator.exprs ?? []);
    case "Aggregate":
      return aggregatedContext(context, operator.group_by ?? []);
    default:
      return cloneContext(context);
  }
}

function sourceContext(schema, tableName, binding) {
  const context = emptyContext(schema);
  const table = findByFold(schema?.node_tables ?? [], tableName);
  if (!table) {
    return context;
  }
  context.nodes.push({ binding, table: table.name });
  context.columns.push(...tableColumns(table, binding));
  return context;
}

function expandedContext(context, operator) {
  const next = cloneContext(context);
  const source = next.nodes.find((node) => sameFold(node.binding, operator.from_binding));
  const rel = findByFold(next.schema?.rel_tables ?? [], operator.rel);
  const target = expandTarget(source?.table, rel, operator.direction);
  const table = findByFold(next.schema?.node_tables ?? [], target);
  if (!table) {
    return next;
  }
  next.nodes.push({ binding: operator.binding, table: table.name });
  next.columns.push(...tableColumns(table, operator.binding));
  return dedupeContext(next);
}

function projectedContext(context, exprs) {
  const next = cloneContext(context);
  const references = exprs.map((item) => item.expr?.col).filter((value) => typeof value === "string");
  next.columns = references
    .map((reference) => choiceByRef(context.columns, reference))
    .filter(Boolean);
  return dedupeContext(next);
}

function aggregatedContext(context, groups) {
  const next = emptyContext(context.schema);
  next.columns = groups
    .map((expr) => expr?.col)
    .filter((value) => typeof value === "string")
    .map((reference) => choiceByRef(context.columns, reference))
    .filter(Boolean);
  return dedupeContext(next);
}

function expandChoices(context) {
  const choices = [];
  for (const node of context.nodes) {
    for (const rel of context.schema?.rel_tables ?? []) {
      if (sameFold(node.table, rel.from)) {
        choices.push(traversalChoice(node.binding, rel, "out", rel.to));
      }
      if (sameFold(node.table, rel.to)) {
        choices.push(traversalChoice(node.binding, rel, "in", rel.from));
      }
    }
  }
  return choices;
}

function traversalChoice(fromBinding, rel, direction, target) {
  const arrow = direction === "out" ? "out →" : "in ←";
  return {
    fromBinding,
    rel,
    direction,
    target,
    label: `${fromBinding} · ${rel.name} · ${arrow} ${target}`,
  };
}

function expandTarget(sourceTable, rel, direction) {
  if (!sourceTable || !rel) {
    return null;
  }
  if (direction === "out" && sameFold(sourceTable, rel.from)) {
    return rel.to;
  }
  if (direction === "in" && sameFold(sourceTable, rel.to)) {
    return rel.from;
  }
  if (direction === "both" && sameFold(sourceTable, rel.from)) {
    return rel.to;
  }
  if (direction === "both" && sameFold(sourceTable, rel.to)) {
    return rel.from;
  }
  return null;
}

function tableColumns(table, binding) {
  return (table.columns ?? []).map((column) => ({
    ref: `${binding}.${column.name}`,
    binding,
    table: table.name,
    name: column.name,
    type: column.type,
  }));
}

function filterColumns(columns) {
  return columns.filter((column) => comparatorsFor(column.type).length > 0);
}

function aggregateColumns(columns, fn) {
  if (fn === "sum" || fn === "avg") {
    return columns.filter((column) => NUMERIC_TYPES.has(column.type));
  }
  return columns;
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

function simplePredicate(predicate) {
  if (!isObject(predicate)) {
    return null;
  }
  const entries = Object.entries(predicate);
  if (entries.length !== 1 || !Object.hasOwn(FILTER_OPERATORS, entries[0][0])) {
    return null;
  }
  const [operator, operands] = entries[0];
  if (!Array.isArray(operands) || operands.length !== 2
    || typeof operands[0]?.col !== "string" || !Object.hasOwn(operands[1] ?? {}, "lit")) {
    return null;
  }
  return { operator, reference: operands[0].col, literal: operands[1].lit };
}

function projectionAliases(choices, currentItems) {
  const current = new Map(currentItems
    .filter((item) => typeof item.expr?.col === "string" && typeof item.as === "string")
    .map((item) => [asciiFold(item.expr.col), item.as]));
  const nameCounts = new Map();
  for (const choice of choices) {
    const key = asciiFold(choice.name);
    nameCounts.set(key, (nameCounts.get(key) ?? 0) + 1);
  }
  const used = new Set();
  return choices.map((choice) => {
    const existing = current.get(asciiFold(choice.ref));
    const base = existing ?? (nameCounts.get(asciiFold(choice.name)) === 1
      ? choice.name
      : `${choice.binding}_${choice.name}`);
    return uniqueName(base, used);
  });
}

function uniqueBinding(tableName, nodes) {
  const used = new Set(nodes.map((node) => asciiFold(node.binding)));
  return uniqueName(bindingForTable(tableName), used);
}

function bindingForTable(tableName) {
  return asciiFold(tableName).replace(/\./g, "_");
}

function invalidBinding(binding) {
  if (binding.length === 0) {
    return "Binding cannot be empty.";
  }
  return binding.includes(".")
    ? "Binding cannot contain a period because column references use binding.column."
    : null;
}

function uniqueName(base, used) {
  let candidate = base;
  let suffix = 2;
  while (used.has(asciiFold(candidate))) {
    candidate = `${base}_${suffix}`;
    suffix += 1;
  }
  used.add(asciiFold(candidate));
  return candidate;
}

function operatorChain(root) {
  const reversed = [];
  let operator = root;
  while (isOperator(operator)) {
    reversed.push(withoutInput(operator));
    operator = operator.input;
  }
  return reversed.reverse();
}

function replaceOperator(envelope, chain, index, operator) {
  const next = chain.map((item, itemIndex) => itemIndex === index ? operator : item);
  return rebuildEnvelope(envelope, next);
}

function removeOperator(envelope, chain, index) {
  return rebuildEnvelope(envelope, chain.filter((_, itemIndex) => itemIndex !== index));
}

function insertOperator(envelope, chain, index, operator) {
  const next = [...chain.slice(0, index + 1), operator, ...chain.slice(index + 1)];
  return rebuildEnvelope(envelope, next);
}

function rebuildEnvelope(envelope, chain) {
  let root = null;
  chain.forEach((operator, index) => {
    root = index === 0 ? withoutInput(operator) : { ...withoutInput(operator), input: root };
  });
  return { ...envelope, plan: root };
}

function withoutInput(operator) {
  return Object.fromEntries(Object.entries(operator).filter(([key]) => key !== INPUT_FIELD));
}

function factList(facts) {
  const list = document.createElement("dl");
  list.className = "tree-facts";
  for (const [term, value] of facts) {
    const name = document.createElement("dt");
    name.textContent = term;
    const description = document.createElement("dd");
    description.textContent = compactValue(value, false);
    list.append(name, description);
  }
  return list;
}

function formElement(label) {
  const element = document.createElement("form");
  element.className = "tree-edit-form";
  element.setAttribute("aria-label", label);
  const body = document.createElement("div");
  body.className = "tree-form-body";
  const error = document.createElement("p");
  error.className = "tree-form-error";
  error.setAttribute("role", "alert");
  error.hidden = true;
  element.append(body, error);
  return { element, body, error };
}

function finishForm(form, submitLabel, submit) {
  const actions = document.createElement("div");
  actions.className = "tree-form-actions";
  const button = document.createElement("button");
  button.className = "button secondary tree-submit-button";
  button.type = "submit";
  button.textContent = submitLabel;
  actions.append(button);
  form.element.append(actions);
  form.element.addEventListener("submit", (event) => {
    event.preventDefault();
    form.error.hidden = true;
    if (!form.element.reportValidity()) {
      return;
    }
    submit();
  });
}

function formFailure(form, message) {
  form.error.textContent = message;
  form.error.hidden = false;
  return false;
}

function unavailableForm(text) {
  const message = document.createElement("p");
  message.className = "tree-unavailable";
  message.textContent = text;
  return message;
}

function controlField(labelText, control) {
  const label = document.createElement("label");
  label.className = "tree-control-field";
  const text = document.createElement("span");
  text.textContent = labelText;
  label.append(text, control);
  return label;
}

function checkboxField(labelText, checkbox) {
  const label = document.createElement("label");
  label.className = "tree-checkbox-field";
  label.append(checkbox, document.createTextNode(labelText));
  return label;
}

function choiceCheckboxes(choices, selected, selectFirst, title = "Columns") {
  const fieldset = document.createElement("fieldset");
  fieldset.className = "tree-choice-list";
  fieldset.append(legend(title));
  const entries = choices.map((choice, index) => {
    const checked = selected.has(asciiFold(choice.ref)) || (selectFirst && index === 0);
    const checkbox = checkboxControl(checked);
    fieldset.append(choiceRow(choice, checkbox));
    return { choice, checkbox };
  });
  return { fieldset, entries };
}

function choiceRow(choice, checkbox) {
  const label = document.createElement("label");
  label.className = "tree-choice-row";
  const name = document.createElement("span");
  name.textContent = choice.ref;
  const type = document.createElement("span");
  type.className = "tree-choice-type";
  type.textContent = choice.type;
  label.append(checkbox, name, type);
  return label;
}

function checkedChoices(entries) {
  return entries.filter((entry) => entry.checkbox.checked).map((entry) => entry.choice);
}

function columnSelect(choices, selected) {
  const select = selectControl(choices.map((choice) => [choice.ref, choiceLabel(choice)]));
  const selectedChoice = choiceByRef(choices, selected);
  if (selectedChoice) {
    select.value = selectedChoice.ref;
  }
  return select;
}

function choiceLabel(choice) {
  return `${choice.ref} · ${choice.type}`;
}

function choiceByRef(choices, reference) {
  return choices.find((choice) => sameFold(choice.ref, reference));
}

function selectControl(options) {
  const select = document.createElement("select");
  replaceOptions(select, options);
  return select;
}

function replaceOptions(select, options) {
  select.replaceChildren();
  for (const optionValue of options) {
    const [value, label] = Array.isArray(optionValue)
      ? optionValue
      : [optionValue, FILTER_OPERATORS[optionValue] ?? optionValue];
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    select.append(option);
  }
}

function literalControl(type, value) {
  if (type === "Bool") {
    return checkboxControl(value === true);
  }
  if (NUMERIC_TYPES.has(type)) {
    const step = type === "Int64" ? "1" : "any";
    return numberControl(typeof value === "number" ? value : 0, step);
  }
  return textControl(typeof value === "string" ? value : "", false);
}

function readLiteral(control, type) {
  if (type === "Bool") {
    return { ok: true, value: control.checked };
  }
  if (NUMERIC_TYPES.has(type)) {
    const value = Number(control.value);
    if (!Number.isFinite(value) || (type === "Int64" && !Number.isSafeInteger(value))) {
      return { ok: false, error: `Enter a valid ${type} value.` };
    }
    return { ok: true, value };
  }
  return { ok: true, value: control.value };
}

function textControl(value, required = true) {
  const input = document.createElement("input");
  input.type = "text";
  input.value = value;
  input.required = required;
  return input;
}

function numberControl(value, step, min) {
  const input = document.createElement("input");
  input.type = "number";
  input.value = String(value);
  input.step = step;
  input.required = true;
  if (min !== undefined) {
    input.min = min;
  }
  return input;
}

function checkboxControl(checked) {
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;
  return input;
}

function legend(text) {
  const element = document.createElement("legend");
  element.textContent = text;
  return element;
}

function formHint(text) {
  const hint = document.createElement("p");
  hint.className = "tree-form-hint";
  hint.textContent = text;
  return hint;
}

function nonnegativeInteger(value) {
  const number = Number(value);
  return Number.isSafeInteger(number) && number >= 0 ? number : null;
}

function appendMutationStatus(card, options, nodeId) {
  if (options.busyNode === nodeId) {
    const status = document.createElement("p");
    status.className = "tree-mutation-status";
    status.setAttribute("role", "status");
    status.textContent = "Checking this edit with the engine…";
    card.append(status);
  }
  if (options.error?.nodeId === nodeId) {
    const error = document.createElement("p");
    error.className = "tree-mutation-error";
    error.setAttribute("role", "alert");
    error.textContent = options.error.message;
    card.append(error);
  }
}

function toggleEditor(toggle, editor) {
  editor.hidden = !editor.hidden;
  toggle.setAttribute("aria-expanded", String(!editor.hidden));
  if (!editor.hidden) {
    editor.querySelector("select, input, button")?.focus();
  }
}

function installRovingTabindex(container) {
  const rovers = Array.from(container.querySelectorAll("[data-tree-rover]"));
  if (rovers.length === 0) {
    return;
  }
  rovers.forEach((control, index) => {
    control.tabIndex = index === 0 ? 0 : -1;
    control.addEventListener("focus", () => setRovingControl(rovers, control));
    control.addEventListener("keydown", (event) => moveRovingFocus(event, rovers, control));
  });
}

function setRovingControl(rovers, active) {
  for (const control of rovers) {
    control.tabIndex = control === active ? 0 : -1;
  }
}

function moveRovingFocus(event, rovers, active) {
  const current = rovers.indexOf(active);
  let next = null;
  if (event.key === "ArrowDown" || event.key === "ArrowRight") {
    next = (current + 1) % rovers.length;
  } else if (event.key === "ArrowUp" || event.key === "ArrowLeft") {
    next = (current - 1 + rovers.length) % rovers.length;
  } else if (event.key === "Home") {
    next = 0;
  } else if (event.key === "End") {
    next = rovers.length - 1;
  }
  if (next !== null) {
    event.preventDefault();
    rovers[next].focus();
  }
}

function treeDisabled(options) {
  return options.disabled === true || typeof options.busyNode === "string";
}

function stepNumber(index) {
  const number = document.createElement("span");
  number.className = "tree-step-number";
  number.textContent = String(index + 1);
  return number;
}

function sectionLabel(text) {
  const label = document.createElement("span");
  label.className = "section-label";
  label.textContent = text;
  return label;
}

function operatorTitle(operator) {
  return STAGE_LABELS[operator.op] ?? (operator.op === "ScanNodes"
    ? "Scan nodes"
    : operator.op === "KnnScan" ? "K-nearest scan" : operator.op);
}

function compactValue(value, quoteStrings = true) {
  if (typeof value === "string" && !quoteStrings) {
    return value;
  }
  if (value === undefined) {
    return "undefined";
  }
  return JSON.stringify(value);
}

function emptyContext(schema) {
  return { schema: validSchema(schema) ? schema : { node_tables: [], rel_tables: [] }, nodes: [], columns: [] };
}

function cloneContext(context) {
  return {
    schema: context.schema,
    nodes: context.nodes.map((node) => ({ ...node })),
    columns: context.columns.map((column) => ({ ...column })),
  };
}

function dedupeContext(context) {
  const seen = new Set();
  context.columns = context.columns.filter((column) => {
    const key = asciiFold(column.ref);
    if (seen.has(key)) {
      return false;
    }
    seen.add(key);
    return true;
  });
  return context;
}

function findByFold(candidates, name) {
  return candidates.find((candidate) => sameFold(candidate.name, name));
}

function sameFold(left, right) {
  return typeof left === "string" && typeof right === "string" && asciiFold(left) === asciiFold(right);
}

function asciiFold(value) {
  return String(value).replace(/[A-Z]/g, (character) => (
    String.fromCharCode(character.charCodeAt(0) + 32)
  ));
}

function validSchema(schema) {
  return schema !== null
    && typeof schema === "object"
    && Array.isArray(schema.node_tables)
    && Array.isArray(schema.rel_tables);
}

function isPlan(value) {
  return isObject(value) && typeof value.v === "number" && isOperator(value.plan);
}

function isOperator(value) {
  return isObject(value) && typeof value.op === "string";
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
