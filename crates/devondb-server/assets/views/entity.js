export function createEntityPanel(onNavigate) {
  const root = document.createElement("aside");
  root.className = "entity-panel";
  root.setAttribute("aria-label", "Entity details");
  root.setAttribute("aria-live", "polite");

  const content = document.createElement("div");
  content.className = "entity-panel-content";
  root.append(content);

  const panel = {
    element: root,
    showEmpty() {
      renderState(content, "Select a graph node to inspect its properties and links.");
    },
    showLoading(node) {
      renderState(content, `Loading ${nodeHeading(node)}…`, "loading-state");
    },
    showError(message) {
      renderState(content, message, "result-error", "alert");
    },
    render(model) {
      renderEntity(content, model, onNavigate);
    },
  };
  panel.showEmpty();
  return Object.freeze(panel);
}

function renderEntity(container, model, onNavigate) {
  const classSummary = nodeClass(model.schema, model.node.table);
  const card = entityCard(model.node, model.schema, classSummary);
  const links = linkedEntities(model, onNavigate);
  container.replaceChildren(card, links);
}

function entityCard(node, schema, classSummary) {
  const card = document.createElement("section");
  card.className = "entity-card";
  const color = classColor(classSummary);
  if (color) {
    card.style.setProperty("--entity-accent", color);
  }

  const heading = document.createElement("header");
  heading.className = "entity-heading";
  const kind = document.createElement("span");
  kind.className = "section-label";
  kind.textContent = classDisplay(classSummary, node.table);
  const title = document.createElement("h3");
  title.textContent = node.label;
  const identity = document.createElement("code");
  identity.className = "entity-identity";
  identity.textContent = node.identityLabel;
  heading.append(kind, title, identity);
  card.append(heading, propertyCard(node, schema));
  return card;
}

function propertyCard(node, schema) {
  const section = document.createElement("section");
  section.className = "entity-section";
  section.append(sectionLabel("properties"));
  const entries = orderedProperties(node, schema);
  if (entries.length === 0) {
    section.append(emptyMessage("No non-key properties."));
    return section;
  }

  const list = document.createElement("dl");
  list.className = "entity-properties";
  for (const [name, value] of entries) {
    const term = document.createElement("dt");
    term.textContent = name;
    const description = document.createElement("dd");
    description.textContent = displayValue(value);
    description.title = description.textContent;
    list.append(term, description);
  }
  section.append(list);
  return section;
}

function linkedEntities(model, onNavigate) {
  const section = document.createElement("section");
  section.className = "entity-section entity-links";
  section.append(sectionLabel("linked entities"));
  if (model.truncated) {
    section.append(truncationNotice());
  }

  const groups = linkGroups(model);
  if (groups.length === 0) {
    section.append(emptyMessage("No linked entities in the returned neighborhood."));
    return section;
  }
  groups.forEach((group, index) => section.append(linkGroup(group, index, onNavigate)));
  return section;
}

function truncationNotice() {
  const notice = document.createElement("p");
  notice.className = "entity-truncation";
  const badge = document.createElement("span");
  badge.className = "warning-badge";
  badge.textContent = "truncated";
  notice.append(badge, document.createTextNode(" Some linked entities may be omitted by the neighborhood cap."));
  return notice;
}

function linkGroups(model) {
  const nodes = new Map(model.nodes.map((node) => [node.id, node]));
  const groups = new Map();
  for (const edge of model.edges) {
    if (edge.source === model.node.id) {
      addLink(groups, edge, "out", nodes.get(edge.target), model.schema);
    }
    if (edge.target === model.node.id) {
      addLink(groups, edge, "in", nodes.get(edge.source), model.schema);
    }
  }
  return Array.from(groups.values());
}

function addLink(groups, edge, direction, neighbor, schema) {
  if (!neighbor) {
    return;
  }
  const key = JSON.stringify([edge.rel, direction]);
  let group = groups.get(key);
  if (!group) {
    group = {
      title: relationshipTitle(schema, edge.rel, direction),
      entries: [],
    };
    groups.set(key, group);
  }
  group.entries.push(neighbor);
}

function linkGroup(group, index, onNavigate) {
  const details = document.createElement("details");
  details.className = "entity-link-group";
  details.open = index === 0;
  const summary = document.createElement("summary");
  summary.textContent = `${group.title} (${group.entries.length})`;
  const list = document.createElement("ul");
  list.className = "entity-link-list";
  for (const node of group.entries) {
    const item = document.createElement("li");
    item.append(entityLink(node, onNavigate));
    list.append(item);
  }
  details.append(summary, list);
  return details;
}

function entityLink(node, onNavigate) {
  const button = document.createElement("button");
  button.className = "entity-link-button";
  button.type = "button";
  button.setAttribute("aria-label", `Center graph on ${nodeHeading(node)}`);
  const label = document.createElement("span");
  label.className = "entity-link-label";
  label.textContent = node.label;
  const identity = document.createElement("span");
  identity.className = "entity-link-identity";
  identity.textContent = node.identityLabel;
  button.append(label, identity);
  button.addEventListener("click", () => onNavigate?.(node));
  return button;
}

function relationshipTitle(schema, rel, direction) {
  const classes = Array.isArray(schema?.classes?.rel_classes)
    ? schema.classes.rel_classes
    : [];
  const summary = classes.find((item) => sameFold(item?.table, rel));
  const declared = direction === "out" ? summary?.verb : summary?.inverse;
  if (typeof declared === "string" && declared.length > 0) {
    return declared;
  }
  return direction === "out" ? `${rel} →` : `← ${rel}`;
}

function orderedProperties(node, schema) {
  const table = (schema?.node_tables ?? []).find((item) => sameFold(item?.name, node.table));
  if (!Array.isArray(table?.columns)) {
    return Object.entries(node.props);
  }
  const props = new Map(Object.entries(node.props).map(([name, value]) => [asciiFold(name), {
    name,
    value,
  }]));
  const entries = [];
  const seen = new Set();
  for (const column of table.columns) {
    const key = asciiFold(column?.name);
    const property = props.get(key);
    if (column?.primary_key === true) {
      seen.add(key);
      continue;
    }
    if (column?.primary_key !== true && property) {
      entries.push([column.name, property.value]);
      seen.add(key);
    }
  }
  for (const [name, value] of Object.entries(node.props)) {
    if (!seen.has(asciiFold(name))) {
      entries.push([name, value]);
    }
  }
  return entries;
}

function nodeClass(schema, table) {
  const classes = Array.isArray(schema?.classes?.node_classes)
    ? schema.classes.node_classes
    : [];
  return classes.find((item) => sameFold(item?.table, table)) ?? null;
}

function classDisplay(summary, fallback) {
  return typeof summary?.display === "string" && summary.display.length > 0
    ? summary.display
    : fallback;
}

function classColor(summary) {
  return typeof summary?.color === "string" && summary.color.length > 0
    ? summary.color
    : null;
}

function renderState(container, text, className = "empty-state", role = null) {
  const state = document.createElement("p");
  state.className = `entity-state ${className}`;
  state.textContent = text;
  if (role) {
    state.setAttribute("role", role);
  }
  container.replaceChildren(state);
}

function emptyMessage(text) {
  const message = document.createElement("p");
  message.className = "entity-empty";
  message.textContent = text;
  return message;
}

function sectionLabel(text) {
  const label = document.createElement("span");
  label.className = "section-label";
  label.textContent = text;
  return label;
}

function nodeHeading(node) {
  return node.label === node.identityLabel
    ? node.identityLabel
    : `${node.label} · ${node.identityLabel}`;
}

function displayValue(value) {
  if (value === null) {
    return "NULL";
  }
  if (typeof value === "string") {
    return value;
  }
  if (value === undefined) {
    return "undefined";
  }
  return typeof value === "object" ? JSON.stringify(value) : String(value);
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
