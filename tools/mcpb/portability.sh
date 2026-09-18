#!/bin/sh
# Metadata checks only: these assertions do not execute foreign binaries.
set -eu
cd "$(dirname "$0")/../.."
for script in tools/mcpb/*.sh; do sh -n "$script"; done
python3 - <<'PY'
import json
from pathlib import Path
import subprocess

template = Path('tools/mcpb/manifest.json').read_text()
targets = {
    'aarch64-apple-darwin': 'darwin',
    'x86_64-apple-darwin': 'darwin',
    'aarch64-unknown-linux-gnu': 'linux',
    'x86_64-unknown-linux-gnu': 'linux',
    'aarch64-unknown-linux-musl': 'linux',
    'x86_64-unknown-linux-musl': 'linux',
}
for target, platform in targets.items():
    actual = subprocess.check_output(
        ['sh', 'tools/mcpb/platform.sh', target], text=True).strip()
    assert actual == platform, (target, actual)
    manifest = json.loads(template.replace('__VERSION__', '0.1.1')
                          .replace('__TARGET__', target)
                          .replace('__PLATFORM__', actual))
    assert manifest['compatibility']['platforms'] == [platform]
    assert manifest['display_name'] == f'devondb ({target})'
    server = manifest['server']
    assert server['entry_point'] == f'server/{target}/devondb'
    assert server['mcp_config']['command'] == '${__dirname}/' + server['entry_point']
    assert server['mcp_config']['args'] == ['mcp', '${user_config.database}']
for target in ('x86_64-pc-windows-msvc', 'wasm32-unknown-unknown', '', '../bad'):
    result = subprocess.run(['sh', 'tools/mcpb/platform.sh', target],
                            capture_output=True, text=True)
    assert result.returncode != 0, target
print('portability.sh: OK — six target manifests and unsupported-target refusals')
PY
