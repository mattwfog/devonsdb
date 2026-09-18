const RESERVED_WORDS = new Set([
  "and", "or", "not", "true", "false", "null", "as", "by", "asc", "desc",
  "out", "in", "both", "from", "to", "nodes", "expand", "filter", "project",
  "sort", "limit", "offset", "aggregate", "knn", "distance", "cosine", "l2",
  "count", "sum", "min", "max", "avg", "create", "insert", "into", "values",
  "node", "rel", "table", "primary", "key",
]);

export function registerSchemaView(registerView) {
  registerView("Schema", { render: renderSchema });
}

function renderSchema(container, model) {
  container.replaceChildren();
  container.append(schemaSection(model));
  container.append(pinnedPlansSection(model));
  container.append(historySection(model.history, model.actions.selectHistory));
}

function schemaSection(model) {
  const section = document.createElement("section");
  section.className = "sidebar-section";
  section.append(sectionHeader("Schema"));

  if (model.loading && !model.schema) {
    section.append(message("Loading schema…", "loading-state"));
    return section;
  }
  if (!validSchema(model.schema)) {
    section.append(message("Schema is unavailable."));
    return section;
  }

  section.append(tableGroup(
    "node tables",
    model.schema.node_tables,
    (table) => nodeTable(table, model.actions.insertStarter),
  ));
  section.append(tableGroup("rel tables", model.schema.rel_tables, relTable));
  return section;
}

function historySection(history, selectHistory) {
  const section = document.createElement("section");
  section.className = "sidebar-section history-section";
  section.append(sectionHeader("session history — browser local"));

  if (history.length === 0) {
    section.append(message("Executed inputs appear here."));
    return section;
  }

  const list = document.createElement("ol");
  list.className = "history-list";
  for (const entry of history) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.className = "history-item";
    button.type = "button";
    button.title = "Load this executed input into the editor";
    button.addEventListener("click", () => selectHistory(entry.input));

    const input = document.createElement("span");
    input.className = "history-input";
    input.textContent = entry.input;

    const metadata = document.createElement("span");
    metadata.className = "history-meta";
    metadata.textContent = historyMetadata(entry);
    button.append(input, metadata);
    item.append(button);
    list.append(item);
  }
  section.append(list);
  return section;
}

function pinnedPlansSection(model) {
  const section = document.createElement("section");
  section.className = "sidebar-section pinned-plans-section";
  section.append(sectionHeader("Pinned plans"));

  const pins = Array.isArray(model.schema?.pins)
    ? model.schema.pins.filter(validPin)
    : [];
  if (pins.length === 0) {
    section.append(message("Engine-pinned queries appear here."));
    return section;
  }

  const list = document.createElement("ul");
  list.className = "pinned-plan-list";
  for (const pin of pins) {
    list.append(pinnedPlan(pin, model.actions, model.busy));
  }
  section.append(list);
  return section;
}

function pinnedPlan(pin, actions, busy) {
  const item = document.createElement("li");
  item.className = "pinned-plan-item";

  const name = document.createElement("strong");
  name.className = "pinned-plan-name";
  name.textContent = pin.name;

  const canonical = document.createElement("span");
  canonical.className = "pinned-plan-canonical";
  canonical.textContent = pin.canonical;
  canonical.title = pin.canonical;

  const actionBar = document.createElement("div");
  actionBar.className = "pinned-plan-actions";

  const run = document.createElement("button");
  run.className = "pinned-plan-action run-pin-action";
  run.type = "button";
  run.disabled = busy;
  run.textContent = "Run";
  run.title = `Run pinned plan ${pin.name}`;
  run.addEventListener("click", () => actions.runPin(pin));

  const unpin = document.createElement("button");
  unpin.className = "pinned-plan-action unpin-action";
  unpin.type = "button";
  unpin.disabled = busy;
  unpin.textContent = "Unpin";
  unpin.title = `Unpin ${pin.name}`;
  let armed = false;
  unpin.addEventListener("click", () => {
    if (!armed) {
      armed = true;
      unpin.classList.add("confirming");
      unpin.textContent = "Confirm unpin";
      unpin.title = `Click again to unpin ${pin.name}`;
      return;
    }
    actions.unpin(pin.name);
  });

  actionBar.append(run, unpin);
  item.append(name, canonical, actionBar);
  return item;
}

function sectionHeader(title) {
  const header = document.createElement("div");
  header.className = "sidebar-section-header";

  const heading = document.createElement("h2");
  heading.textContent = title;
  header.append(heading);
  return header;
}

function tableGroup(title, tables, renderTable) {
  const group = document.createElement("section");
  group.className = "schema-group";

  const heading = document.createElement("h3");
  heading.textContent = title;
  group.append(heading);

  if (tables.length === 0) {
    group.append(message("none"));
    return group;
  }

  const list = document.createElement("ul");
  list.className = "schema-list";
  for (const table of tables) {
    const item = document.createElement("li");
    item.className = "schema-table";
    item.append(renderTable(table));
    list.append(item);
  }
  group.append(list);
  return group;
}

function nodeTable(table, insertStarter) {
  const fragment = document.createDocumentFragment();
  const starter = nodeStarter(table.name);

  const button = document.createElement("button");
  button.className = "schema-table-button";
  button.type = "button";
  button.title = `Insert ${starter}`;
  button.addEventListener("click", () => insertStarter(starter));

  const name = document.createElement("span");
  name.className = "schema-table-name";
  name.textContent = table.name;

  const hint = document.createElement("span");
  hint.className = "schema-insert-hint";
  hint.textContent = "+ editor";
  button.append(name, hint);

  fragment.append(button, columnList(table.columns));
  return fragment;
}

function relTable(table) {
  const fragment = document.createDocumentFragment();
  const heading = document.createElement("div");
  heading.className = "schema-table-button";

  const name = document.createElement("span");
  name.className = "schema-table-name";
  name.textContent = table.name;

  const endpoints = document.createElement("span");
  endpoints.className = "rel-endpoints";
  endpoints.textContent = `${table.from} → ${table.to}`;
  heading.append(name, endpoints);

  fragment.append(heading, columnList(table.columns));
  return fragment;
}

function columnList(columns) {
  const list = document.createElement("ul");
  list.className = "column-list";
  if (!Array.isArray(columns) || columns.length === 0) {
    const item = document.createElement("li");
    item.className = "column-row";
    item.textContent = "no properties";
    list.append(item);
    return list;
  }

  for (const column of columns) {
    const item = document.createElement("li");
    item.className = "column-row";

    const name = document.createElement("span");
    name.className = "column-name";
    name.textContent = column.name;
    if (column.primary_key) {
      const key = document.createElement("span");
      key.className = "primary-key-mark";
      key.textContent = "PK";
      name.append(key);
    }

    const type = document.createElement("span");
    type.textContent = column.type;
    item.append(name, type);
    list.append(item);
  }
  return list;
}

function nodeStarter(tableName) {
  return `nodes(${planIdentifier(tableName)}) as ${initialBinding(tableName)}`;
}

function planIdentifier(name) {
  if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) && !RESERVED_WORDS.has(name)) {
    return name;
  }
  return `\`${escapeIdentifier(name)}\``;
}

function escapeIdentifier(name) {
  let escaped = "";
  for (const character of name) {
    if (character === "`") {
      escaped += "\\`";
    } else if (character === "\\") {
      escaped += "\\\\";
    } else if (character === "\n") {
      escaped += "\\n";
    } else if (character === "\r") {
      escaped += "\\r";
    } else if (character === "\t") {
      escaped += "\\t";
    } else if (character.codePointAt(0) < 0x20) {
      escaped += `\\u{${character.codePointAt(0).toString(16)}}`;
    } else {
      escaped += character;
    }
  }
  return escaped;
}

function initialBinding(tableName) {
  const first = Array.from(tableName)[0] ?? "n";
  return /^[A-Za-z]$/.test(first) ? first.toLowerCase() : "n";
}

function historyMetadata(entry) {
  const kind = entry.kind === "statement" ? "statement" : "query";
  if (!entry.executed_at) {
    return kind;
  }
  const date = new Date(entry.executed_at);
  return Number.isNaN(date.valueOf()) ? kind : `${kind} · ${date.toLocaleString()}`;
}

function validSchema(schema) {
  return schema !== null
    && typeof schema === "object"
    && Array.isArray(schema.node_tables)
    && Array.isArray(schema.rel_tables);
}

function validPin(pin) {
  return pin !== null
    && typeof pin === "object"
    && typeof pin.name === "string"
    && typeof pin.text === "string"
    && typeof pin.canonical === "string";
}

function message(text, className = "empty-state") {
  const paragraph = document.createElement("p");
  paragraph.className = className;
  paragraph.textContent = text;
  return paragraph;
}
