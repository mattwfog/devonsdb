import { createEntityPanel } from "./entity.js";

const SVG_NAMESPACE = ["http:", "", "www.w3.org", "2000", "svg"].join("/");
const VIEW_WIDTH = 1_000;
const VIEW_HEIGHT = 500;
const EARTH_RADIUS_METERS = 6_371_008.8;
const MIN_ZOOM = 1;
const MAX_ZOOM = 8;
const MAP_BINDING = "map";
const controllers = new WeakMap();

export function registerMapView(registerView) {
  registerView("Map", { render: renderMap });
}

export function mapViewContext(schema, explanation) {
  if (explanation?.kind !== "query") {
    return null;
  }
  const source = sourceOperator(explanation.plan?.plan);
  if (!source || typeof source.table !== "string") {
    return null;
  }
  const table = findTable(schema?.node_tables, source.table);
  const geoColumns = (table?.columns ?? []).filter((column) => column?.type === "GeoPoint");
  if (!table || geoColumns.length === 0) {
    return null;
  }
  const withinColumn = source.op === "WithinScan"
    ? geoColumns.find((column) => sameFold(column.name, source.column))
    : null;
  return {
    table,
    column: withinColumn ?? geoColumns[0],
  };
}

export function createMapRequest(context) {
  if (!context?.table?.name || !context?.column?.name) {
    return null;
  }
  return {
    table: context.table.name,
    column: context.column.name,
    plan: {
      v: 0,
      plan: {
        op: "ScanNodes",
        table: context.table.name,
        binding: MAP_BINDING,
      },
    },
  };
}

function renderMap(container, model) {
  let controller = controllers.get(container);
  if (!controller) {
    controller = createMapController(container);
    controllers.set(container, controller);
  }
  controller.attach(model);
}

function createMapController(container) {
  const dom = mapDom();
  const state = {
    dom,
    container,
    model: null,
    contextKey: null,
    zoom: MIN_ZOOM,
    focus: { x: VIEW_WIDTH / 2, y: VIEW_HEIGHT / 2 },
    entityRequest: 0,
    entity: null,
  };
  state.entity = createEntityPanel((node) => void loadEntity(state, node));
  dom.layout.append(dom.canvas, state.entity.element);
  bindControls(state);

  return Object.freeze({
    attach(model) {
      const key = contextKey(model?.map);
      if (key !== state.contextKey) {
        state.contextKey = key;
        state.zoom = MIN_ZOOM;
        state.focus = { x: VIEW_WIDTH / 2, y: VIEW_HEIGHT / 2 };
        state.entityRequest += 1;
        state.entity.showEmpty();
      }
      state.model = model;
      if (dom.root.parentNode !== container) {
        container.replaceChildren(dom.root);
      }
      draw(state);
    },
  });
}

function mapDom() {
  const root = document.createElement("section");
  root.className = "map-view";

  const toolbar = document.createElement("div");
  toolbar.className = "map-toolbar";
  const summary = document.createElement("div");
  summary.className = "result-summary map-summary";
  const controls = document.createElement("div");
  controls.className = "map-zoom-controls";
  controls.setAttribute("aria-label", "Map zoom controls");
  const zoomOut = controlButton("−", "Zoom out");
  const zoomLabel = document.createElement("output");
  zoomLabel.className = "map-zoom-label";
  zoomLabel.setAttribute("aria-live", "polite");
  const zoomIn = controlButton("+", "Zoom in");
  const reset = controlButton("World", "Reset to the world view");
  reset.classList.add("map-reset-button");
  controls.append(zoomOut, zoomLabel, zoomIn, reset);
  toolbar.append(summary, controls);

  const feedback = document.createElement("div");
  feedback.className = "map-feedback";
  feedback.setAttribute("aria-live", "polite");

  const layout = document.createElement("div");
  layout.className = "map-layout";
  const canvas = mapCanvas();
  root.append(toolbar, feedback, layout);
  return {
    root,
    toolbar,
    summary,
    controls,
    zoomOut,
    zoomLabel,
    zoomIn,
    reset,
    feedback,
    layout,
    ...canvas,
  };
}

function mapCanvas() {
  const canvas = document.createElement("div");
  canvas.className = "map-canvas";
  const svg = svgElement("svg");
  svg.setAttribute("viewBox", `0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Equirectangular map of stored GeoPoint values");
  svg.setAttribute("focusable", "false");

  const background = svgElement("rect");
  background.classList.add("map-background");
  background.setAttribute("width", String(VIEW_WIDTH));
  background.setAttribute("height", String(VIEW_HEIGHT));
  const graticule = svgElement("g");
  graticule.classList.add("map-graticule");
  appendGraticule(graticule);
  const overlay = svgElement("g");
  overlay.classList.add("map-within-overlay");
  const points = svgElement("g");
  points.classList.add("map-points");
  svg.append(background, graticule, overlay, points);

  const empty = document.createElement("p");
  empty.className = "map-empty";
  empty.hidden = true;
  canvas.append(svg, empty);
  return { canvas, svg, overlay, points, empty };
}

function bindControls(state) {
  state.dom.zoomOut.addEventListener("click", () => changeZoom(state, -1));
  state.dom.zoomIn.addEventListener("click", () => changeZoom(state, 1));
  state.dom.reset.addEventListener("click", () => {
    state.zoom = MIN_ZOOM;
    state.focus = { x: VIEW_WIDTH / 2, y: VIEW_HEIGHT / 2 };
    draw(state);
  });
  state.dom.svg.addEventListener("wheel", (event) => {
    event.preventDefault();
    changeZoom(state, event.deltaY < 0 ? 1 : -1);
  }, { passive: false });
}

function changeZoom(state, delta, focus = state.focus) {
  const next = clamp(state.zoom + delta, MIN_ZOOM, MAX_ZOOM);
  if (next === state.zoom) {
    return;
  }
  state.zoom = next;
  state.focus = focus;
  draw(state);
}

function draw(state) {
  const model = state.model;
  const map = model?.map;
  state.dom.zoomOut.disabled = state.zoom === MIN_ZOOM;
  state.dom.zoomIn.disabled = state.zoom === MAX_ZOOM;
  state.dom.zoomLabel.value = `zoom ${state.zoom}`;
  setViewBox(state);
  renderFeedback(state, map);
  state.dom.overlay.replaceChildren();
  state.dom.points.replaceChildren();

  const points = mapPoints(map, model?.schema);
  const overlay = withinOverlay(model, map);
  applyMatches(points, overlay);
  renderSummary(state, map, points, overlay);
  renderWithinOverlay(state, overlay);
  const clusters = clusterPoints(points, state.zoom);
  renderClusters(state, expandMaxZoomClusters(clusters, state.zoom));
  renderEmptyState(state, map, points);
}

function renderFeedback(state, map) {
  state.dom.feedback.replaceChildren();
  if (!map?.error) {
    return;
  }
  const error = document.createElement("p");
  error.className = "result-error";
  error.setAttribute("role", "alert");
  error.textContent = map.error;
  state.dom.feedback.append(error);
}

function renderSummary(state, map, points, overlay) {
  state.dom.summary.replaceChildren();
  const table = map?.table;
  const column = map?.column;
  const text = document.createElement("span");
  if (!table || !column) {
    text.textContent = "Run a query against a GeoPoint-bearing table to open the map.";
  } else {
    text.textContent = `${table.name}.${column.name} · ${points.length} mapped ${plural(points.length, "point")}`;
  }
  state.dom.summary.append(text);
  if (overlay) {
    const match = document.createElement("span");
    match.className = "map-match-count";
    match.textContent = `${overlay.matches.size} within match${overlay.matches.size === 1 ? "" : "es"}`;
    state.dom.summary.append(match);
  }
  if (map?.result?.truncated === true) {
    const warning = document.createElement("span");
    warning.className = "warning-badge";
    warning.textContent = "truncated by server";
    state.dom.summary.append(warning);
  }
}

function renderEmptyState(state, map, points) {
  let message = null;
  if (map?.loading) {
    message = "Loading stored GeoPoint rows…";
  } else if (!map?.table) {
    message = "This query does not target a table with a GeoPoint column.";
  } else if (!mapResultReady(map)) {
    message = map?.error ? "Map rows could not be loaded." : "Waiting for map rows…";
  } else if (points.length === 0) {
    message = `No non-null ${map.column.name} points were returned.`;
  }
  state.dom.empty.hidden = message === null;
  state.dom.empty.textContent = message ?? "";
  state.dom.svg.classList.toggle("is-empty", message !== null);
}

function mapPoints(map, schema) {
  if (!mapResultReady(map)) {
    return [];
  }
  const table = map.table;
  const result = map.result;
  const geoIndex = resultColumnIndex(result.columns, map.column.name);
  const primaryKey = table.columns.find((column) => column.primary_key === true);
  const keyIndex = resultColumnIndex(result.columns, primaryKey?.name);
  if (geoIndex < 0 || keyIndex < 0) {
    return [];
  }
  return result.rows.flatMap((row) => {
    const geo = geoValue(Array.isArray(row) ? row[geoIndex] : null);
    if (!geo) {
      return [];
    }
    const key = row[keyIndex];
    const props = rowProperties(result.columns, row, primaryKey?.name);
    const identityLabel = `${table.name}:${displayValue(key)}`;
    return [{
      table: table.name,
      key,
      props,
      lat: geo.lat,
      lng: geo.lng,
      id: nodeIdentity(table.name, key),
      identityLabel,
      label: nodeDisplayLabel(table.name, props, identityLabel, schema),
      matched: false,
    }];
  });
}

function withinOverlay(model, map) {
  const within = findWithinOperator(model?.explanation?.plan?.plan);
  if (!within || !map?.table || !map?.column
      || !sameFold(within.table, map.table.name)
      || !sameFold(within.column, map.column.name)) {
    return null;
  }
  const center = rawGeoPoint(within.center);
  if (!center || !Number.isFinite(within.meters) || within.meters <= 0) {
    return null;
  }
  return {
    center,
    meters: within.meters,
    matches: model?.resultCanonical === model?.explanation?.canonical
      ? matchedIdentities(model?.result?.data, map.table)
      : new Set(),
  };
}

function matchedIdentities(result, table) {
  const matches = new Set();
  const primaryKey = table.columns.find((column) => column.primary_key === true);
  if (!validQueryResult(result) || !primaryKey) {
    return matches;
  }
  const keyIndex = resultColumnIndex(result.columns, primaryKey.name);
  if (keyIndex < 0) {
    return matches;
  }
  for (const row of result.rows) {
    if (Array.isArray(row)) {
      matches.add(nodeIdentity(table.name, row[keyIndex]));
    }
  }
  return matches;
}

function applyMatches(points, overlay) {
  if (!overlay) {
    return;
  }
  for (const point of points) {
    point.matched = overlay.matches.has(point.id);
  }
}

function clusterPoints(points, zoom) {
  const clusters = new Map();
  for (const point of points) {
    const cell = devonGridDisplayCell(point, zoom);
    let cluster = clusters.get(cell);
    if (!cluster) {
      cluster = { points: [], x: 0, y: 0, matched: 0 };
      clusters.set(cell, cluster);
    }
    const projected = project(point);
    cluster.points.push(point);
    cluster.x += projected.x;
    cluster.y += projected.y;
    cluster.matched += point.matched ? 1 : 0;
  }
  return Array.from(clusters.values(), (cluster) => ({
    ...cluster,
    x: cluster.x / cluster.points.length,
    y: cluster.y / cluster.points.length,
  }));
}

function devonGridDisplayCell(point, zoom) {
  // Presentation-only DevonGrid approximation: truncate displayed coordinates
  // into a congruent grid whose density doubles with zoom. Query membership
  // remains the engine's exact DevonGrid/great-circle result.
  const longitudeCells = 2 ** (zoom + 2);
  const latitudeCells = longitudeCells / 2;
  const longitude = clamp(
    Math.trunc(((point.lng + 180) / 360) * longitudeCells),
    0,
    longitudeCells - 1,
  );
  const latitude = clamp(
    Math.trunc(((point.lat + 90) / 180) * latitudeCells),
    0,
    latitudeCells - 1,
  );
  return `${longitude}:${latitude}`;
}

function expandMaxZoomClusters(clusters, zoom) {
  if (zoom !== MAX_ZOOM) {
    return clusters;
  }
  const spacing = 18 / 2 ** (zoom - 1);
  return clusters.flatMap((cluster) => {
    if (cluster.points.length === 1) {
      return [cluster];
    }
    const columns = Math.ceil(Math.sqrt(cluster.points.length));
    const rows = Math.ceil(cluster.points.length / columns);
    return cluster.points.map((point, index) => ({
      points: [point],
      x: cluster.x + (index % columns - (columns - 1) / 2) * spacing,
      y: cluster.y + (Math.floor(index / columns) - (rows - 1) / 2) * spacing,
      matched: point.matched ? 1 : 0,
    }));
  });
}

function renderClusters(state, clusters) {
  const unit = 1 / 2 ** (state.zoom - 1);
  for (const cluster of clusters) {
    const marker = svgElement("g");
    marker.classList.add("map-marker");
    if (cluster.matched === cluster.points.length && cluster.matched > 0) {
      marker.classList.add("matched");
    } else if (cluster.matched > 0) {
      marker.classList.add("partially-matched");
    }
    marker.setAttribute("role", "button");
    marker.setAttribute("tabindex", "0");
    marker.setAttribute("transform", `translate(${cluster.x} ${cluster.y})`);
    marker.setAttribute("aria-label", clusterLabel(cluster));

    const ring = svgElement("circle");
    ring.classList.add("map-point-ring");
    ring.setAttribute("r", String((cluster.points.length > 1 ? 12 : 8) * unit));
    const core = svgElement("circle");
    core.classList.add("map-point-core");
    core.setAttribute("r", String((cluster.points.length > 1 ? 8 : 5) * unit));
    marker.append(ring, core);
    if (cluster.points.length > 1) {
      const count = svgElement("text");
      count.classList.add("map-cluster-count");
      count.setAttribute("font-size", String(9 * unit));
      count.setAttribute("dy", String(3 * unit));
      count.textContent = String(cluster.points.length);
      marker.append(count);
    }
    const title = svgElement("title");
    title.textContent = clusterTitle(cluster);
    marker.append(title);
    marker.addEventListener("click", () => activateCluster(state, cluster));
    marker.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        activateCluster(state, cluster);
      }
    });
    state.dom.points.append(marker);
  }
}

function activateCluster(state, cluster) {
  if (cluster.points.length > 1 && state.zoom < MAX_ZOOM) {
    changeZoom(state, 1, { x: cluster.x, y: cluster.y });
    return;
  }
  void loadEntity(state, cluster.points[0]);
}

function renderWithinOverlay(state, overlay) {
  if (!overlay) {
    return;
  }
  const projected = project(overlay.center);
  const ellipse = radiusEllipse(overlay.center, overlay.meters);
  for (const offset of [-VIEW_WIDTH, 0, VIEW_WIDTH]) {
    const disc = svgElement("ellipse");
    disc.classList.add("map-radius-disc");
    disc.setAttribute("cx", String(projected.x + offset));
    disc.setAttribute("cy", String(projected.y));
    disc.setAttribute("rx", String(ellipse.rx));
    disc.setAttribute("ry", String(ellipse.ry));
    state.dom.overlay.append(disc);
  }
  const unit = 1 / 2 ** (state.zoom - 1);
  const marker = svgElement("g");
  marker.classList.add("map-center-marker");
  marker.setAttribute("transform", `translate(${projected.x} ${projected.y})`);
  const stem = svgElement("path");
  stem.setAttribute("d", `M 0 0 L 0 ${13 * unit}`);
  const pin = svgElement("circle");
  pin.setAttribute("r", String(6 * unit));
  const center = svgElement("circle");
  center.classList.add("map-center-core");
  center.setAttribute("r", String(2 * unit));
  const title = svgElement("title");
  title.textContent = `Within center ${overlay.center.lat}, ${overlay.center.lng}; radius ${overlay.meters} m`;
  marker.append(stem, pin, center, title);
  state.dom.overlay.append(marker);
}

function radiusEllipse(center, meters) {
  // Small-circle approximation in the equirectangular projection: latitude
  // uses angular radius directly; longitude expands by cos(latitude).
  const angularDegrees = (meters / EARTH_RADIUS_METERS) * (180 / Math.PI);
  const cosine = Math.max(Math.abs(Math.cos(center.lat * Math.PI / 180)), 0.05);
  return {
    rx: Math.min((angularDegrees / cosine) * (VIEW_WIDTH / 360), VIEW_WIDTH * 2),
    ry: Math.min(angularDegrees * (VIEW_HEIGHT / 180), VIEW_HEIGHT),
  };
}

async function loadEntity(state, point) {
  if (!point) {
    return;
  }
  const request = state.entityRequest + 1;
  state.entityRequest = request;
  state.entity.showLoading(point);
  try {
    const response = await requestJson("/api/graph", {
      table: point.table,
      key: point.key,
      limit: 500,
    });
    if (request !== state.entityRequest) {
      return;
    }
    const graph = normalizeGraphResponse(response, state.model?.schema);
    const node = graph.nodes.find((item) => item.id === point.id);
    if (!node) {
      throw new Error("The server returned a neighborhood without the selected entity.");
    }
    state.entity.render({ node, ...graph, schema: state.model?.schema });
  } catch (error) {
    if (request === state.entityRequest) {
      state.entity.showError(errorMessage(error));
    }
  }
}

async function requestJson(path, body) {
  if (!path.startsWith("/api/")) {
    throw new Error(`Refusing non-API request path: ${path}`);
  }
  const response = await fetch(path, {
    method: "POST",
    headers: { Accept: "application/json", "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  let payload;
  try {
    payload = text.length === 0 ? null : JSON.parse(text);
  } catch (error) {
    throw new Error(`Invalid JSON from ${path}: ${errorMessage(error)}`);
  }
  if (!response.ok) {
    throw new Error(typeof payload?.error === "string" ? payload.error : `${response.status} ${response.statusText}`);
  }
  return payload;
}

function normalizeGraphResponse(value, schema) {
  if (!isObject(value) || !Array.isArray(value.nodes) || !Array.isArray(value.edges)
      || typeof value.truncated !== "boolean") {
    throw new Error("The server returned a malformed graph response.");
  }
  const nodes = value.nodes.map((node) => normalizeNode(node, schema));
  const nodeIds = new Set(nodes.map((node) => node.id));
  const edges = value.edges.map((edge) => normalizeEdge(edge, nodeIds));
  return { nodes, edges, truncated: value.truncated };
}

function normalizeNode(value, schema) {
  if (!isObject(value) || typeof value.table !== "string"
      || !Object.hasOwn(value, "key") || !isObject(value.props)) {
    throw new Error("The server returned a malformed graph node.");
  }
  const id = nodeIdentity(value.table, value.key);
  const identityLabel = `${value.table}:${displayValue(value.key)}`;
  return {
    id,
    table: value.table,
    key: value.key,
    props: value.props,
    identityLabel,
    label: nodeDisplayLabel(value.table, value.props, identityLabel, schema),
  };
}

function normalizeEdge(value, nodeIds) {
  if (!isObject(value) || typeof value.rel !== "string") {
    throw new Error("The server returned a malformed graph edge.");
  }
  const source = endpointIdentity(value.from);
  const target = endpointIdentity(value.to);
  if (!source || !target || !nodeIds.has(source) || !nodeIds.has(target)) {
    throw new Error("The server returned a graph edge with an unknown endpoint.");
  }
  return {
    id: JSON.stringify([value.rel, source, target]),
    rel: value.rel,
    source,
    target,
  };
}

function endpointIdentity(value) {
  return isObject(value) && typeof value.table === "string" && Object.hasOwn(value, "key")
    ? nodeIdentity(value.table, value.key)
    : null;
}

function rowProperties(columns, row, primaryKey) {
  const props = {};
  columns.forEach((label, index) => {
    const name = resultColumnName(label);
    if (name && !sameFold(name, primaryKey)) {
      props[name] = row[index];
    }
  });
  return props;
}

function nodeDisplayLabel(table, props, fallback, schema) {
  const summary = nodeClass(schema, table);
  if (typeof summary?.label === "string") {
    const entry = Object.entries(props).find(([name]) => sameFold(name, summary.label));
    return entry && entry[1] !== null ? displayValue(entry[1]) : fallback;
  }
  for (const preferred of ["name", "title", "label"]) {
    const entry = Object.entries(props).find(([name, value]) => (
      asciiFold(name) === preferred && typeof value === "string"
    ));
    if (entry) {
      return entry[1];
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

function setViewBox(state) {
  const scale = 2 ** (state.zoom - 1);
  const width = VIEW_WIDTH / scale;
  const height = VIEW_HEIGHT / scale;
  const x = clamp(state.focus.x - width / 2, 0, VIEW_WIDTH - width);
  const y = clamp(state.focus.y - height / 2, 0, VIEW_HEIGHT - height);
  state.focus = { x: x + width / 2, y: y + height / 2 };
  state.dom.svg.setAttribute("viewBox", `${x} ${y} ${width} ${height}`);
}

function appendGraticule(group) {
  for (let longitude = -150; longitude <= 150; longitude += 30) {
    const x = project({ lat: 0, lng: longitude }).x;
    group.append(line(x, 0, x, VIEW_HEIGHT));
  }
  for (let latitude = -60; latitude <= 60; latitude += 30) {
    const y = project({ lat: latitude, lng: 0 }).y;
    group.append(line(0, y, VIEW_WIDTH, y));
  }
  const equator = line(0, VIEW_HEIGHT / 2, VIEW_WIDTH, VIEW_HEIGHT / 2);
  equator.classList.add("map-equator");
  group.append(equator);
}

function line(x1, y1, x2, y2) {
  const element = svgElement("line");
  element.setAttribute("x1", String(x1));
  element.setAttribute("y1", String(y1));
  element.setAttribute("x2", String(x2));
  element.setAttribute("y2", String(y2));
  return element;
}

function project(point) {
  return {
    x: ((point.lng + 180) / 360) * VIEW_WIDTH,
    y: ((90 - point.lat) / 180) * VIEW_HEIGHT,
  };
}

function mapResultReady(map) {
  return validQueryResult(map?.result)
    && sameFold(map.resultTable, map.table?.name)
    && sameFold(map.resultColumn, map.column?.name);
}

function findWithinOperator(operator) {
  let current = operator;
  while (isObject(current)) {
    if (current.op === "WithinScan") {
      return current;
    }
    current = current.input;
  }
  return null;
}

function sourceOperator(operator) {
  let current = operator;
  while (isObject(current?.input)) {
    current = current.input;
  }
  return isObject(current) && ["ScanNodes", "KnnScan", "WithinScan"].includes(current.op)
    ? current
    : null;
}

function rawGeoPoint(value) {
  return isObject(value)
    && Number.isFinite(value.lat_deg)
    && Number.isFinite(value.lng_deg)
    ? { lat: value.lat_deg, lng: value.lng_deg }
    : null;
}

function geoValue(value) {
  return rawGeoPoint(value?.geo);
}

function resultColumnIndex(columns, name) {
  if (!Array.isArray(columns) || typeof name !== "string") {
    return -1;
  }
  return columns.findIndex((label) => sameFold(resultColumnName(label), name));
}

function resultColumnName(label) {
  if (typeof label !== "string") {
    return null;
  }
  const separator = label.indexOf(".");
  return separator < 0 ? label : label.slice(separator + 1);
}

function validQueryResult(value) {
  return isObject(value) && Array.isArray(value.columns) && Array.isArray(value.rows);
}

function contextKey(map) {
  return map?.table?.name && map?.column?.name
    ? JSON.stringify([asciiFold(map.table.name), asciiFold(map.column.name)])
    : null;
}

function findTable(tables, name) {
  return (Array.isArray(tables) ? tables : []).find((table) => sameFold(table?.name, name));
}

function clusterLabel(cluster) {
  if (cluster.points.length === 1) {
    return `Open ${cluster.points[0].identityLabel}`;
  }
  return `${cluster.points.length} points; zoom in`;
}

function clusterTitle(cluster) {
  if (cluster.points.length === 1) {
    const point = cluster.points[0];
    return `${point.label} · ${point.lat}, ${point.lng}`;
  }
  return `${cluster.points.length} points in this display cell`;
}

function controlButton(text, label) {
  const button = document.createElement("button");
  button.className = "button map-control-button";
  button.type = "button";
  button.textContent = text;
  button.setAttribute("aria-label", label);
  return button;
}

function plural(count, singular) {
  return count === 1 ? singular : `${singular}s`;
}

function nodeIdentity(table, key) {
  return JSON.stringify([table, key]);
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

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
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

function isObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function clamp(value, minimum, maximum) {
  return Math.max(minimum, Math.min(maximum, value));
}

function svgElement(name) {
  return document.createElementNS(SVG_NAMESPACE, name);
}
