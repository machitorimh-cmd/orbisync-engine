"""Prepare ef3ed18 SDK + generated protocol under ignored local cache for compatibility testing."""
import subprocess
from pathlib import Path

BASE = 'ef3ed18e5d58a88c902cf2bed07f1bc8f32e3d38'
REPOSITORY = Path(__file__).resolve().parent.parent
CACHE = REPOSITORY / 'sdk/typescript/node_modules/.cache/sdk04-old'
(CACHE / 'proto/orbisync/v1').mkdir(parents=True, exist_ok=True)
for filename in ['client.ts', 'uuidv7.ts', 'realtime_ticket.ts']:
    (CACHE / filename).write_bytes(subprocess.check_output(
        ['git', 'show', f'{BASE}:sdk/typescript/src/{filename}'], cwd=REPOSITORY))
(CACHE / 'proto/orbisync/v1/realtime.proto').write_bytes(subprocess.check_output(
    ['git', 'show', f'{BASE}:proto/orbisync/v1/realtime.proto'], cwd=REPOSITORY))
(CACHE / 'package.json').write_text('{"type":"module"}', encoding='utf-8')
(CACHE / 'buf.gen.yaml').write_text(
    'version: v2\nplugins:\n  - local: protoc-gen-es\n    out: '
    + (CACHE / 'generated').as_posix() + '\n    opt: [target=ts]\n', encoding='utf-8')
subprocess.run(['buf', 'generate', str(CACHE / 'proto'), '--template',
                str(CACHE / 'buf.gen.yaml')], cwd=REPOSITORY, check=True)
print(f'Prepared unchanged legacy SDK and generated schema from {BASE}')
