#!/usr/bin/env python3
"""Build unsigned, isolated macOS host/client apps for native acceptance.
Requires macOS 14+ for explicit persistent WebKit data-store separation.
Does not launch apps, install them, publish, or alter the production data folder.
"""
import json
import os
from pathlib import Path
import platform
import subprocess

ROOT = Path(__file__).resolve().parents[1]
if platform.system() != 'Darwin' or int(platform.mac_ver()[0].split('.')[0]) < 14:
    raise SystemExit('Acceptance builds require macOS 14 or newer.')
env = os.environ.copy()
env['PATH'] = str(Path.home() / '.cargo/bin') + os.pathsep + env['PATH']
base = json.loads((ROOT / 'src/app/tauri.conf.json').read_text())
for role in ('host', 'client'):
    identifier = f'com.blackwall.acceptance.{role}'
    window = dict(base['app']['windows'][0])
    window['title'] = f'Blackwall Acceptance — {role.title()}'
    config = {
        'identifier': identifier,
        'productName': f'Blackwall Acceptance {role.title()}',
        'app': {'windows': [window]},
        'bundle': {'createUpdaterArtifacts': False},
        'plugins': {'updater': {'endpoints': []}},
    }
    subprocess.run([str(ROOT / 'ui/node_modules/.bin/tauri'), 'build', '--bundles', 'app',
                    '--no-sign', '--ci', '--config', json.dumps(config)],
                   cwd=ROOT / 'src/app', env=env, check=True)
print('Acceptance host/client bundles are in src/target/release/bundle/macos/.')
