"""Exercise the native archive through its manifest, with bounded subprocesses."""

import json
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import zipfile


def run(command, text=None):
    """Run a native command and report its output if it fails or times out."""
    result = subprocess.run(command, input=text, text=True, capture_output=True,
                            timeout=60, check=True)
    return result.stdout


def unpack(bundle, destination, target, version):
    """Check metadata and safely unpack only the documented archive members."""
    with zipfile.ZipFile(bundle) as archive:
        entry = f'server/{target}/devondb'
        members = archive.namelist()
        assert len(members) == len(set(members)), 'duplicate zip member'
        files = {name for name in members if not name.endswith('/')}
        assert files == {'manifest.json', entry, 'LICENSE', 'THIRD_PARTY_LICENSES.md'}, files
        notices = Path('crates/devondb-geo/THIRD_PARTY_LICENSES.md').read_bytes()
        assert archive.read('THIRD_PARTY_LICENSES.md') == notices, 'third-party notices changed'
        directories = {name for name in members if name.endswith('/')}
        assert directories <= {'server/', f'server/{target}/'}, directories
        manifest = json.loads(archive.read('manifest.json'))
        platform = run(['sh', 'tools/mcpb/platform.sh', target]).strip()
        assert manifest['manifest_version'] == '0.3'
        assert manifest['version'] == version
        assert manifest['compatibility']['platforms'] == [platform]
        assert manifest['display_name'] == f'devondb ({target})'
        assert manifest['server']['type'] == 'binary'
        assert manifest['server']['entry_point'] == entry
        assert manifest['user_config']['database']['type'] == 'file'
        assert manifest['user_config']['database']['required'] is True
        info = archive.getinfo(entry)
        mode = info.external_attr >> 16
        assert stat.S_ISREG(mode) and mode & 0o111, 'binary is not executable'
        assert 0 < info.file_size <= 10485760, 'binary size budget exceeded'
        archive.extractall(destination)
        # Python's zip extractor does not restore the archived executable bits.
        (destination / entry).chmod(mode & 0o777)
    return manifest


def exchange(manifest, directory, database):
    """Resolve host substitutions and exercise discovery, ask and write refusal."""
    config = manifest['server']['mcp_config']
    assert config['command'] == '${__dirname}/' + manifest['server']['entry_point']
    assert config['args'] == ['mcp', '${user_config.database}']
    assert config.get('env', {}) == {}
    command = [config['command'], *config['args']]
    command = [token.replace('${__dirname}', str(directory))
               .replace('${user_config.database}', str(database)) for token in command]
    requests = [
        ('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                        'clientInfo': {'name': 'mcpb-smoke', 'version': '1'}}),
        ('tools/list', {}),
        ('tools/call', {'name': 'ask', 'arguments': {'question': 'who does ada know'}}),
        ('tools/call', {'name': 'query', 'arguments': {
            'text': 'insert into Person values (4, "forbidden")'}}),
    ]
    lines = [json.dumps({'jsonrpc': '2.0', 'id': index, 'method': method,
                         'params': params})
             for index, (method, params) in enumerate(requests, 1)]
    replies = [json.loads(line) for line in run(command, '\n'.join(lines) + '\n').splitlines()]
    assert [reply['id'] for reply in replies] == [1, 2, 3, 4], replies
    assert all(reply['jsonrpc'] == '2.0' and 'error' not in reply for reply in replies)
    assert replies[0]['result']['protocolVersion'] == '2025-06-18'
    names = {tool['name'] for tool in replies[1]['result']['tools']}
    assert names == {'schema', 'ask', 'query', 'explain'}, names
    answer = replies[2]['result']
    assert answer['isError'] is False, answer
    text = answer['content'][0]['text']
    assert text.startswith('plan: ') and 'Grace' in text and 'Linus' in text, text
    refusal = replies[3]['result']
    assert refusal['isError'] is True and 'read-only' in refusal['content'][0]['text']
    return text


def main():
    """Seed and query a database using only the extracted distribution binary."""
    bundle = Path(sys.argv[1]).resolve()
    target = re.search(r'^host: (.+)$', run(['rustc', '-vV']), re.MULTILINE)[1]
    version = re.search(r'^version = "([0-9.]+)"$', Path('Cargo.toml').read_text(),
                        re.MULTILINE)[1]
    assert bundle.name == f'devondb-{version}-{target}.mcpb', bundle.name
    build = Path('tools/mcpb/build').resolve()
    build.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='smoke space ', dir=build) as temporary:
        directory = Path(temporary) / 'unpacked space'
        manifest = unpack(bundle, directory, target, version)
        binary = directory / manifest['server']['entry_point']
        assert run([str(binary), '--version']).strip() == f'devondb {version}'
        database = Path(temporary) / 'company space.devondb'
        seed = '\n'.join([
            'create node table Person (id Int64 primary key, name String)',
            'create rel table Knows from Person to Person',
            'insert into Person values (1, "ada"), (2, "Grace"), (3, "Linus")',
            'insert rel into Knows values (1 -> 2), (1 -> 3)', '.exit', '',
        ])
        assert run([str(binary), str(database)], seed).strip() == 'ok\nok\nok\nok'
        answer = exchange(manifest, directory, database)
        rows = run([str(binary), str(database)], 'nodes(Person) as p\n.exit\n')
        assert 'forbidden' not in rows, rows
        print(f'smoke.sh: OK — {target} packed MCP tools, ask and write refusal')
        print(answer.splitlines()[0])


if __name__ == '__main__':
    try:
        main()
    except (AssertionError, KeyError, ValueError, OSError, subprocess.SubprocessError) as error:
        print(f'smoke.sh: FAIL: {error}', file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stdout, error.stderr, file=sys.stderr)
        sys.exit(1)
