const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const MIN_DISTANCE_SQUARED = 16;

const DEFAULTS = Object.freeze({
  timeStep: 0.65,
  repulsion: 5_200,
  springStrength: 0.025,
  restLength: 88,
  centering: 0.003,
  damping: 0.86,
  initialSpacing: 18,
});

export function createForceSimulation(inputNodes, inputEdges, options = {}) {
  const config = simulationConfig(options);
  const nodes = inputNodes.map((node, index) => simulationNode(node, index, config));
  const nodesById = new Map(nodes.map((node) => [node.id, node]));
  const edges = inputEdges.map((edge) => simulationEdge(edge, nodesById));

  return Object.freeze({
    nodes,
    edges,
    tick() {
      return tick(nodes, edges, config);
    },
    kineticEnergy() {
      return kineticEnergy(nodes);
    },
    pin(id, x, y) {
      const node = nodesById.get(id);
      if (node) {
        pinNode(node, x, y);
      }
      return node;
    },
    unpin(id) {
      const node = nodesById.get(id);
      if (node) {
        node.pinned = false;
      }
      return node;
    },
    unpinAll() {
      for (const node of nodes) {
        node.pinned = false;
      }
    },
  });
}

function simulationConfig(options) {
  return Object.freeze({
    timeStep: positiveOption(options.timeStep, DEFAULTS.timeStep),
    repulsion: positiveOption(options.repulsion, DEFAULTS.repulsion),
    springStrength: positiveOption(options.springStrength, DEFAULTS.springStrength),
    restLength: positiveOption(options.restLength, DEFAULTS.restLength),
    centering: positiveOption(options.centering, DEFAULTS.centering),
    damping: boundedOption(options.damping, DEFAULTS.damping, 0, 1),
    initialSpacing: positiveOption(options.initialSpacing, DEFAULTS.initialSpacing),
  });
}

function simulationNode(input, index, config) {
  const seeded = seededPosition(index, config.initialSpacing);
  const hasPosition = Number.isFinite(input.x) && Number.isFinite(input.y);
  const pinned = input.pinned === true;
  const x = hasPosition ? input.x : seeded.x;
  const y = hasPosition ? input.y : seeded.y;

  return {
    id: input.id,
    x,
    y,
    vx: Number.isFinite(input.vx) ? input.vx : 0,
    vy: Number.isFinite(input.vy) ? input.vy : 0,
    pinned,
    fx: pinned && Number.isFinite(input.fx) ? input.fx : x,
    fy: pinned && Number.isFinite(input.fy) ? input.fy : y,
  };
}

function simulationEdge(input, nodesById) {
  const source = nodesById.get(input.source);
  const target = nodesById.get(input.target);
  if (!source || !target) {
    throw new TypeError("Every force edge must reference two simulation nodes.");
  }
  return { source, target };
}

function seededPosition(index, spacing) {
  const radius = spacing * Math.sqrt(index);
  const angle = index * GOLDEN_ANGLE;
  return {
    x: Math.cos(angle) * radius,
    y: Math.sin(angle) * radius,
  };
}

function tick(nodes, edges, config) {
  const before = accelerations(nodes, edges, config);
  const halfTimeSquared = 0.5 * config.timeStep * config.timeStep;

  for (let index = 0; index < nodes.length; index += 1) {
    integratePosition(nodes[index], before[index], config.timeStep, halfTimeSquared);
  }

  const after = accelerations(nodes, edges, config);
  for (let index = 0; index < nodes.length; index += 1) {
    integrateVelocity(nodes[index], before[index], after[index], config);
  }
  return kineticEnergy(nodes);
}

function integratePosition(node, acceleration, timeStep, halfTimeSquared) {
  if (node.pinned) {
    node.x = node.fx;
    node.y = node.fy;
    node.vx = 0;
    node.vy = 0;
    return;
  }
  node.x += node.vx * timeStep + acceleration.x * halfTimeSquared;
  node.y += node.vy * timeStep + acceleration.y * halfTimeSquared;
}

function integrateVelocity(node, before, after, config) {
  if (node.pinned) {
    node.vx = 0;
    node.vy = 0;
    return;
  }
  const halfTime = 0.5 * config.timeStep;
  node.vx = (node.vx + (before.x + after.x) * halfTime) * config.damping;
  node.vy = (node.vy + (before.y + after.y) * halfTime) * config.damping;
}

function accelerations(nodes, edges, config) {
  const values = Array.from(nodes, () => ({ x: 0, y: 0 }));
  applyRepulsion(nodes, values, config.repulsion);
  applySprings(edges, values, nodes, config);
  applyCentering(nodes, values, config.centering);
  return values;
}

function applyRepulsion(nodes, values, strength) {
  for (let left = 0; left < nodes.length; left += 1) {
    for (let right = left + 1; right < nodes.length; right += 1) {
      const delta = separation(nodes[left], nodes[right], left, right);
      const scale = strength / (delta.distanceSquared * Math.sqrt(delta.distanceSquared));
      const x = delta.x * scale;
      const y = delta.y * scale;
      values[left].x -= x;
      values[left].y -= y;
      values[right].x += x;
      values[right].y += y;
    }
  }
}

function separation(leftNode, rightNode, leftIndex, rightIndex) {
  const x = rightNode.x - leftNode.x;
  const y = rightNode.y - leftNode.y;
  const distanceSquared = x * x + y * y;
  if (distanceSquared >= MIN_DISTANCE_SQUARED) {
    return { x, y, distanceSquared };
  }

  const angle = (leftIndex + 1) * GOLDEN_ANGLE + (rightIndex + 1);
  return {
    x: Math.cos(angle) * Math.sqrt(MIN_DISTANCE_SQUARED),
    y: Math.sin(angle) * Math.sqrt(MIN_DISTANCE_SQUARED),
    distanceSquared: MIN_DISTANCE_SQUARED,
  };
}

function applySprings(edges, values, nodes, config) {
  const indexByNode = new Map(nodes.map((node, index) => [node, index]));
  for (const edge of edges) {
    const x = edge.target.x - edge.source.x;
    const y = edge.target.y - edge.source.y;
    const distance = Math.max(Math.hypot(x, y), 0.001);
    const scale = config.springStrength * (distance - config.restLength) / distance;
    const forceX = x * scale;
    const forceY = y * scale;
    const sourceIndex = indexByNode.get(edge.source);
    const targetIndex = indexByNode.get(edge.target);
    values[sourceIndex].x += forceX;
    values[sourceIndex].y += forceY;
    values[targetIndex].x -= forceX;
    values[targetIndex].y -= forceY;
  }
}

function applyCentering(nodes, values, strength) {
  for (let index = 0; index < nodes.length; index += 1) {
    values[index].x -= nodes[index].x * strength;
    values[index].y -= nodes[index].y * strength;
  }
}

function kineticEnergy(nodes) {
  let total = 0;
  for (const node of nodes) {
    if (!node.pinned) {
      total += 0.5 * (node.vx * node.vx + node.vy * node.vy);
    }
  }
  return total;
}

function pinNode(node, x, y) {
  if (!Number.isFinite(x) || !Number.isFinite(y)) {
    return;
  }
  node.pinned = true;
  node.fx = x;
  node.fy = y;
  node.x = x;
  node.y = y;
  node.vx = 0;
  node.vy = 0;
}

function positiveOption(value, fallback) {
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

function boundedOption(value, fallback, minimum, maximum) {
  return Number.isFinite(value) && value > minimum && value < maximum ? value : fallback;
}
