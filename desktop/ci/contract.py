"""Check Cargo package boundaries using the same resolved dependency graphs as the builds."""
import argparse
from functools import cache
import json
import re
import tomllib
import subprocess
from pathlib import Path

repo = Path(__file__).resolve().parents[2]


@cache
def metadata_for(manifest):
    return json.loads(subprocess.check_output([
        "cargo", "metadata", "--format-version", "1", "--locked",
        "--manifest-path", str(repo / manifest),
    ]))


def dependency_names(manifest, package):
    metadata = metadata_for(manifest)
    packages = {p["id"]: p["name"] for p in metadata["packages"]}
    nodes = {node["id"]: node["dependencies"] for node in metadata["resolve"]["nodes"]}
    pending = [p for p in metadata["workspace_members"] if packages[p] == package]
    if len(pending) != 1:
        raise ValueError(f"Expected one workspace member named {package}")
    seen = set()
    while pending:
        current = pending.pop()
        if current not in seen:
            seen.add(current)
            pending.extend(nodes[current])
    return {packages[p] for p in seen}


for manifest, package in [("Cargo.toml", "ox"), ("desktop/Cargo.toml", "ox-desk-core"),
                          ("desktop/Cargo.toml", "ox-desk-probe"),
                          *(("desktop/Cargo.toml", name) for name in ("ox-host", "ox-host-service", "ox-host-web", "ox-host-protocol", "ox-pty"))]:
    gpui = sorted(n for n in dependency_names(manifest, package) if n.startswith("gpui"))
    if gpui:
        raise SystemExit(f"{package} unexpectedly depends on {gpui}")

gateway = dependency_names("Cargo.toml", "ox")
if any(p.startswith(("ox-host", "ox-desk", "ox-pty")) for p in gateway):
    raise SystemExit("The standalone gateway must stay independent of desktop and session host packages")

# Resolve just the small admin client. Workspace-wide cargo metadata unifies it
# with the daemon's separate server feature and would falsely include the web stack.
web_client = subprocess.check_output([
    "cargo", "tree", "--locked", "--manifest-path", str(repo / "desktop/ox-host-web/Cargo.toml"),
    "-p", "ox-host-web", "--no-default-features", "-e", "normal", "--prefix", "none", "-f", "{p}",
], text=True)
if re.search(r"^(?:axum|tokio|web-push-native|ox-desk-core) v", web_client, re.M):
    raise SystemExit("The default local administration client pulls in the host's web server")

parser = argparse.ArgumentParser()
parser.add_argument("--final", action="store_true")
final = parser.parse_args().final
desktop = repo / "desktop"
metadata = metadata_for("desktop/Cargo.toml")
assert Path(metadata["workspace_root"]) == desktop and (desktop / "Cargo.lock").is_file()
app_manifest = tomllib.loads((desktop / "ox-desktop/Cargo.toml").read_text())
obsolete = {"tokio", "async-channel", "anyhow"} & app_manifest["dependencies"].keys()
if obsolete:
    raise SystemExit(f"Legacy desktop dependencies: {sorted(obsolete)}")
core = next(p for p in metadata["packages"] if p["name"] == "ox-desk-core")
# tempfile provides private scratch directories and atomic title-cache writes.
allowed = {"serde", "serde_json", "libc", "notify", "tempfile", "schemars", "ox-memory"}
bad = {d["name"] for d in core["dependencies"] if d["kind"] is None} - allowed
if bad:
    raise SystemExit(f"Unexpected production dependencies in ox-desk-core: {sorted(bad)}")

legacy = tomllib.loads((desktop / "ci/migration.toml").read_text())["legacy_wire"]
if final and legacy:
    raise SystemExit(f"Pending desktop migration contracts: {legacy}")
app = desktop / "ox-desktop/src"
for source in app.rglob("*.rs"):
    relative = str(source.relative_to(app))
    if relative.startswith("views/") and re.search(r"\b(?:codex|claude)\s*::", source.read_text()):
        raise SystemExit(f"{relative} must use the runtime-neutral workbench interface")
    if source.name == "tests.rs":
        parent = source.parent.with_suffix(".rs")
        if parent.is_file() and "#[cfg(test)]\nmod tests;" in parent.read_text():
            continue
    if relative.startswith("platform/") or relative == "platform.rs":
        continue
    code = source.read_text().split("#[cfg(test)]", 1)[0]
    if re.search(r'["\'](?:CODEX_HOME|\.codex|auth\.json|sessions/rollout)', code):
        raise SystemExit(f"{relative} may not open a Codex-owned path")
    if relative in legacy:
        continue
    if re.search(r"serde_json::(?:Value|\{[^}]*\bValue\b)|\bCommand::new\(|std::fs::write\(", code):
        raise SystemExit(f"{relative} needs a typed core or platform operation")
    if '"codeox.toml"' in code:
        raise SystemExit(f"{relative} must edit provider settings through OX")

for source in (desktop / "ox-desk-core/src").rglob("*.rs"):
    relative = str(source.relative_to(desktop / "ox-desk-core/src"))
    code = source.read_text().split("#[cfg(test)]", 1)[0]
    # Discovery passes the selected home into stock Codex and reads thread/list.
    # It cannot open rollout or credential files itself.
    paths_code = code.replace('.env("CODEX_HOME", home)', '') if relative == "native_catalog.rs" else code
    if re.search(r'["\'](?:CODEX_HOME|\.codex|auth\.json)', paths_code):
        raise SystemExit(f"{relative} may not open a Codex-owned path")
    if "Command::new(" in code and relative not in ("links/codex.rs", "links/claude.rs", "ports/ox.rs", "native_catalog.rs", "codex/installed.rs"):
        raise SystemExit(f"{relative} must invoke a typed OX port or runtime link")

from claude_catalog_contract import check as check_claude_catalog
check_claude_catalog(desktop)
