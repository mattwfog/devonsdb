import { renderQueryTree } from "./tree.js";

const INPUT_FIELD = "input";

export function registerPlanView(registerView) {
  registerView("Plan", { render: renderPlan });
}

function renderPlan(container, model) {
  container.replaceChildren();

  if (model.loading && !model.explanation) {
    container.append(message("Explaining DevonPlan…", "loading-state"));
    return;
  }

  if (!model.explanation) {
    container.append(message("Explain a query or statement to inspect its canonical plan."));
    return;
  }

  container.append(canonicalBlock(model.explanation.canonical));
  const tree = model.explanation.kind === "query"
    ? renderQueryTree(model.explanation.plan, model.schema, {
      disabled: model.loading,
      busyNode: model.treeBusyNode,
      error: model.treeError,
      onMutate: model.actions?.mutateTree,
    })
    : null;
  container.append(tree ?? planOutline(model.explanation.plan));
}

function canonicalBlock(canonical) {
  const block = document.createElement("section");
  block.className = "canonical-block";

  const label = document.createElement("span");
  label.className = "section-label";
  label.textContent = "canonical text";

  const text = document.createElement("pre");
  text.className = "canonical-text";
  text.textContent = canonical;

  block.append(label, text);
  return block;
}

function planOutline(canonicalJson) {
  const outline = document.createElement("section");
  outline.className = "plan-tree";

  const label = document.createElement("span");
  label.className = "section-label";
  label.textContent = "operator outline";
  outline.append(label);

  if (!isObject(canonicalJson)) {
    outline.append(leaf("plan", canonicalJson));
    return outline;
  }

  const rootKey = Object.hasOwn(canonicalJson, "plan")
    ? "plan"
    : Object.hasOwn(canonicalJson, "statement")
      ? "statement"
      : null;

  appendMetadata(outline, canonicalJson, rootKey);
  const root = rootKey === null ? canonicalJson : canonicalJson[rootKey];
  outline.append(valueBranch(rootKey ?? "plan", root, 0));
  return outline;
}

function appendMetadata(outline, canonicalJson, rootKey) {
  const metadata = Object.entries(canonicalJson).filter(([key]) => key !== rootKey);
  if (metadata.length === 0) {
    return;
  }

  const line = document.createElement("p");
  line.className = "plan-leaf";
  line.textContent = metadata
    .map(([key, value]) => `${key}=${compactValue(value)}`)
    .join(" · ");
  outline.append(line);
}

function valueBranch(label, value, depth) {
  if (isOperator(value)) {
    return operatorBranch(value, depth);
  }
  if (!isObject(value) && !Array.isArray(value)) {
    return leaf(label, value);
  }
  return collectionBranch(label, value, depth);
}

function operatorBranch(operator, depth) {
  const details = document.createElement("details");
  details.className = "operator-node";
  details.open = depth < 5;

  const summary = document.createElement("summary");
  const name = document.createElement("strong");
  name.className = "operator-name";
  name.textContent = operator.op;
  summary.append(name);

  const fields = Object.entries(operator).filter(
    ([key]) => key !== "op" && key !== INPUT_FIELD,
  );
  if (fields.length > 0) {
    summary.append(operatorFields(fields));
  }
  details.append(summary);

  if (Object.hasOwn(operator, INPUT_FIELD)) {
    details.append(valueBranch(INPUT_FIELD, operator[INPUT_FIELD], depth + 1));
  }
  return details;
}

function operatorFields(fields) {
  const container = document.createElement("span");
  container.className = "operator-fields";

  for (const [key, value] of fields) {
    const field = document.createElement("span");
    field.className = "operator-field";

    const name = document.createElement("span");
    name.className = "field-name";
    name.textContent = key;
    field.append(name, document.createTextNode(`=${compactValue(value)}`));
    container.append(field);
  }
  return container;
}

function collectionBranch(label, value, depth) {
  const details = document.createElement("details");
  details.className = "plan-branch";
  details.open = depth < 3;

  const summary = document.createElement("summary");
  const count = Array.isArray(value) ? ` (${value.length})` : "";
  summary.textContent = `${label}${count}`;
  details.append(summary);

  for (const [key, child] of Object.entries(value)) {
    details.append(valueBranch(key, child, depth + 1));
  }
  return details;
}

function leaf(label, value) {
  const line = document.createElement("p");
  line.className = "plan-leaf";
  line.textContent = `${label}=${compactValue(value)}`;
  return line;
}

function compactValue(value) {
  if (typeof value === "string") {
    return JSON.stringify(value);
  }
  if (value === undefined) {
    return "undefined";
  }
  return JSON.stringify(value);
}

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isOperator(value) {
  return isObject(value) && typeof value.op === "string";
}

function message(text, className = "empty-state") {
  const paragraph = document.createElement("p");
  paragraph.className = className;
  paragraph.textContent = text;
  return paragraph;
}
