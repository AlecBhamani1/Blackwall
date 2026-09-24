#!/usr/bin/env python3
"""Launch one isolated acceptance bundle against a running local fixture."""
import argparse
import json
from pathlib import Path
import subprocess
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("role", choices=("host", "client"))
parser.add_argument("--directory", type=Path, required=True)
args = parser.parse_args()
directory = args.directory.resolve()
state = json.loads((directory / "state.json").read_text())
for key in ("modelEndpoint", "relayUrl"):
    url = urlparse(state[key])
    if url.scheme != "http" or url.hostname != "127.0.0.1" or not url.port:
        raise SystemExit("Acceptance fixtures must use loopback HTTP addresses.")
bundle = ROOT / "src/target/release/bundle/macos" / f"Blackwall Acceptance {args.role.title()}.app"
if not bundle.is_dir():
    raise SystemExit("Build the acceptance apps with scripts/build-acceptance.py first.")
# The client has no direct model route: successful chat must use its saved pairing.
endpoint = state["modelEndpoint"] if args.role == "host" else "http://127.0.0.1:1/v1"
subprocess.run([
    "/usr/bin/open", "-n", str(bundle),
    "--env", "BLACKWALL_MODEL_ENDPOINT=" + endpoint,
    "--env", "BLACKWALL_RELAY_URL=" + state["relayUrl"],
    "--env", "BLACKWALL_MODEL_API_KEY=",
    "--env", "BLACKWALL_RELAY_TOKEN=",
    "--env", "OLLAMA_HOST=",
    "-o", str(directory / f"{args.role}.stdout"),
    "--stderr", str(directory / f"{args.role}.stderr"),
], check=True)
