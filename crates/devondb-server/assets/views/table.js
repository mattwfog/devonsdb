const ROW_LIMIT = 1_000;

export function registerTableView(registerView) {
  registerView("Table", { render: renderTable });
}

function renderTable(container, model) {
  container.replaceChildren();

  if (model.loading && !model.result) {
    container.append(message("Running canonical plan…", "loading-state"));
    return;
  }
  if (!model.result) {
    container.append(message("Run a query to see rows."));
    return;
  }
  if (model.result.kind === "statement") {
    container.append(statementResult());
    return;
  }

  renderQueryResult(container, model.result.data);
}

function renderQueryResult(container, result) {
  if (!isQueryResult(result)) {
    container.append(message("The server returned a malformed query result.", "result-error"));
    return;
  }

  const rowCount = totalRows(result);
  const visibleRows = result.rows.slice(0, ROW_LIMIT);
  const columns = result.columns.map(String);

  container.append(resultSummary(rowCount, result.truncated));
  if (columns.length === 0) {
    container.append(message("The query returned no columns."));
  } else {
    container.append(tableElement(columns, visibleRows));
  }

  if (rowCount > ROW_LIMIT || result.rows.length > ROW_LIMIT) {
    const footer = document.createElement("p");
    footer.className = "table-footer";
    footer.textContent = `showing ${visibleRows.length} of ${rowCount}`;
    container.append(footer);
  }
}

function resultSummary(rowCount, truncated) {
  const summary = document.createElement("div");
  summary.className = "result-summary";

  const count = document.createElement("span");
  count.textContent = `(${rowCount} rows)`;
  summary.append(count);

  if (truncated) {
    const warning = document.createElement("span");
    warning.className = "warning-badge";
    warning.textContent = "truncated by server";
    summary.append(warning);
  }
  return summary;
}

function tableElement(columns, rows) {
  const scroll = document.createElement("div");
  scroll.className = "table-scroll";

  const table = document.createElement("table");
  table.className = "result-table";
  table.append(tableHead(columns), tableBody(columns, rows));
  scroll.append(table);
  return scroll;
}

function tableHead(columns) {
  const head = document.createElement("thead");
  const row = document.createElement("tr");

  for (const column of columns) {
    const cell = document.createElement("th");
    cell.scope = "col";
    cell.textContent = column;
    row.append(cell);
  }
  head.append(row);
  return head;
}

function tableBody(columns, rows) {
  const body = document.createElement("tbody");
  for (const values of rows) {
    const row = document.createElement("tr");
    for (let index = 0; index < columns.length; index += 1) {
      row.append(valueCell(Array.isArray(values) ? values[index] : undefined));
    }
    body.append(row);
  }
  return body;
}

function valueCell(value) {
  const cell = document.createElement("td");
  if (value === null) {
    cell.className = "null-value";
    cell.textContent = "NULL";
    return cell;
  }

  cell.textContent = displayValue(value);
  cell.title = cell.textContent;
  return cell;
}

function displayValue(value) {
  if (value === undefined) {
    return "";
  }
  if (typeof value === "object") {
    return JSON.stringify(value);
  }
  return String(value);
}

function statementResult() {
  const result = document.createElement("div");
  result.className = "statement-result";

  const text = document.createElement("span");
  text.textContent = "statement executed";
  result.append(text);
  return result;
}

function totalRows(result) {
  const declared = Number.isSafeInteger(result.row_count) && result.row_count >= 0
    ? result.row_count
    : result.rows.length;
  return Math.max(declared, result.rows.length);
}

function isQueryResult(value) {
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
