#!/usr/bin/env python3
"""Install the repository's optional Lua plugins and their support files."""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path
from pathlib import PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / "plugins/catalog.json"
MAX_FILE_BYTES = 1024 * 1024


def relative_path(value):
    if not isinstance(value, str) or not value or "\\" in value:
        raise ValueError(f"Invalid package path: {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or str(path) != value:
        raise ValueError(f"Package path must be relative and canonical: {value!r}")
    return path


def source_path(value):
    path = ROOT.joinpath(relative_path(value)).resolve()
    if not path.is_relative_to(ROOT) or not path.is_file():
        raise ValueError(f"Missing source or source outside the catalog: {value!r}")
    if path.stat().st_size > MAX_FILE_BYTES:
        raise ValueError(f"Plugin source exceeds 1 MB: {value}")
    return path


def read_catalog():
    catalog = json.loads(CATALOG.read_text())
    if catalog.get("version") != 1 or not isinstance(catalog.get("plugins"), dict):
        raise ValueError("Unsupported plugin catalog")
    plugins = catalog["plugins"]
    private_catalog = ROOT / "private/catalog.json"
    if private_catalog.exists():
        private = json.loads(private_catalog.read_text())
        if private.get("version") != 1 or not isinstance(private.get("plugins"), dict):
            raise ValueError("Unsupported private plugin catalog")
        for name, plugin in private["plugins"].items():
            if name in plugins or not isinstance(plugin, dict) or plugin.get("kind") != "private":
                raise ValueError(f"Invalid or duplicate private plugin: {name}")
            plugins[name] = plugin
    if len(plugins) > 64:
        raise ValueError("The deck supports at most 64 plugins")
    targets = set()
    registered = set()
    for name, plugin in plugins.items():
        if not re.fullmatch(r"[a-zA-Z0-9_-]{1,32}", name):
            raise ValueError(f"Invalid plugin name: {name!r}")
        if not isinstance(plugin, dict) or not isinstance(plugin.get("description"), str):
            raise ValueError(f"Missing description: {name}")
        if plugin.get("kind") == "builtin":
            source_path(plugin["source"])
            continue
        if plugin.get("kind") not in {"optional", "helper", "private"} or not isinstance(plugin.get("files"), list):
            raise ValueError(f"Invalid package: {name}")
        main = f"plugins/{name}.lua"
        private = plugin["kind"] == "private"
        entry = f"private/{main}" if private else main
        for payload in plugin["files"]:
            source = source_path(payload["source"])
            if payload["source"].startswith("private/") != private:
                raise ValueError(f"Plugin source crosses the public/private boundary: {name}")
            target = str(relative_path(payload["target"]))
            if not target.startswith(("plugins/", "lua/", "workspaces/", "service/")) or target in targets:
                raise ValueError(f"Unsupported or duplicate install target: {target}")
            targets.add(target)
            if payload["source"] == entry and target == main:
                registered.add(source)
        if plugin.get("kind") != "helper" and (ROOT / entry).resolve() not in registered:
            raise ValueError(f"Package {name} must include {entry}")
    for name, plugin in plugins.items():
        requires = plugin.get("requires", [])
        if not isinstance(requires, list) or any(
            not isinstance(required, str)
            or plugins.get(required, {}).get("kind") not in {"optional", "helper", "private"}
            for required in requires
        ):
            raise ValueError(f"Package {name} requires an unknown or built-in package")
    discovered = {path.resolve() for path in (ROOT / "plugins").glob("*.lua")}
    discovered.update(path.resolve() for path in (ROOT / "private/plugins").glob("*.lua"))
    if discovered != registered:
        raise ValueError("Every plugins/*.lua file must be registered in catalog.json")
    # Source archives have no Git metadata; a development checkout must track
    # every executable payload before it can be installed or selected as source.
    checkout = any((directory / ".git").exists() for directory in (ROOT, *ROOT.parents))
    if checkout:
        tracked = subprocess.run(
            ["git", "-C", str(ROOT), "ls-files", "--full-name", "-z"], capture_output=True, check=True
        )
        top = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        )
        repository = Path(top.stdout.strip())
        tracked_paths = {repository / os.fsdecode(p) for p in tracked.stdout.split(b"\0") if p}
        sources = {source_path(p["source"]) for plugin in plugins.values() for p in plugin.get("files", [])}
        sources.update(source_path(plugin["source"]) for plugin in plugins.values() if "source" in plugin)
        missing = sorted(str(path.relative_to(ROOT)) for path in sources if path not in tracked_paths)
        if missing:
            raise ValueError("Plugin sources are not tracked by Git: " + ", ".join(missing))
    return plugins


def default_config_dir():
    configured = os.environ.get("CEPHEUS_CONFIG") or os.environ.get("AGENT_DECK_CONFIG")  # legacy-name
    if configured:
        return Path(configured).expanduser().parent
    base = Path(os.environ.get("XDG_CONFIG_HOME") or "~/.config").expanduser()
    current = base / "cepheus"
    legacy = base / "agent-deck"  # legacy-name
    return legacy if not current.exists() and legacy.is_dir() else current


def destination_path(config, relative):
    destination = config / relative
    # Resolve the profile root once, but never replace a linked plugin or follow
    # linked subdirectories out into another application's files.
    for part in (destination, *destination.parents):
        if part == config:
            break
        if part.is_symlink():
            raise ValueError(f"Refusing linked install target: {part}")
        if part.exists() and part != destination and not part.is_dir():
            raise ValueError(f"Install parent is not a directory: {part}")
    if destination.exists() and not destination.is_file():
        raise ValueError(f"Install target is not a file: {destination}")
    return destination


def with_requirements(plugins, names):
    """The named packages and every package they require, each once."""
    selected, pending = [], list(names)
    while pending:
        name = pending.pop(0)
        if name not in selected:
            selected.append(name)
            pending.extend(plugins[name].get("requires", []))
    return selected


def install(plugins, names, config, *, update=False, dry_run=False):
    plan = []
    for name in names:
        plugin = plugins[name]
        if plugin["kind"] == "builtin":
            print(f"{name}: built into the deck; no profile copy needed")
            continue
        for payload in plugin["files"]:
            source = source_path(payload["source"])
            data = source.read_bytes()
            destination = destination_path(config, payload["target"])
            exists = destination.exists()
            if exists and destination.read_bytes() == data:
                continue
            if exists and not update:
                raise ValueError(
                    f"Different file at {destination}; use --update to back it up and replace it"
                )
            plan.append((destination, data, exists))

    # All selected files have passed conflict and path checks before any write.
    for destination, data, exists in plan:
        print(f"{'Update' if exists else 'Install'} {destination}")
        if dry_run:
            continue
        destination_path(config, str(destination.relative_to(config)))
        destination.parent.mkdir(parents=True, exist_ok=True)
        if exists:
            backup = destination.with_name(
                f"{destination.name}.before-plugin-update-{uuid.uuid4().hex}"
            )
            shutil.copy2(destination, backup)
            print(f"  Previous file: {backup}")
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(
                prefix=".plugin-install-", dir=destination.parent, delete=False
            ) as stream:
                temporary = Path(stream.name)
                stream.write(data)
            temporary.chmod(0o644)
            os.replace(temporary, destination)
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
    if not plan:
        print("Selected plugins are up to date.")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("names", nargs="*", help="Catalog plugin names")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--list", action="store_true", help="List available plugins")
    mode.add_argument(
        "--check", action="store_true", help="Validate the catalog without installing"
    )
    mode.add_argument("--all", action="store_true", help="Install all optional plugins")
    mode.add_argument("--repo-sources", action="store_true", help="Load plugins directly from this checkout in this profile")
    parser.add_argument("--config-dir", type=Path, default=default_config_dir())
    parser.add_argument("--dry-run", action="store_true", help="Show changes without writing")
    parser.add_argument(
        "--update", action="store_true", help="Back up and replace differing installed files"
    )
    args = parser.parse_args(argv)
    try:
        plugins = read_catalog()
        if args.names and (args.list or args.check or args.all or args.repo_sources):
            raise ValueError("Choose plugin names or one catalog mode")
        if args.list:
            for name, plugin in sorted(plugins.items()):
                print(f"{name} ({plugin['kind']}): {plugin['description']}")
            return 0
        if args.check:
            print(f"Plugin catalog valid: {len(plugins)} plugins")
            return 0
        if args.repo_sources:
            config = args.config_dir.expanduser().resolve()
            targets = {payload["target"] for plugin in plugins.values() for payload in plugin.get("files", [])}
            missing = [str(path.relative_to(config)) for directory in ("plugins", "service")
                       for path in (config / directory).glob("*.lua")
                       if str(path.relative_to(config)) not in targets]
            if missing:
                raise ValueError("Installed plugins lack repository catalog sources; integrate them before switching: "
                                 + ", ".join(sorted(missing)))
            path = destination_path(config, "profile.json")
            profile_bytes = path.read_bytes() if path.exists() else None
            profile = json.loads(profile_bytes) if profile_bytes is not None else {}
            if not isinstance(profile, dict):
                raise ValueError("The local profile must be an object")
            profile["plugin_source_dir"] = str(ROOT)
            data = (json.dumps(profile, indent=2) + "\n").encode()
            # Native registrations are validated and published by the current
            # binary under its existing controller lock; never execute words
            # taken from the provider manifest.
            dot_path = config / "providers/dot.json"
            if dot_path.exists():
                with dot_path.open("rb") as stream:
                    raw = stream.read(8 * 1024 * 1024 + 1)
                if len(raw) > 8 * 1024 * 1024:
                    raise ValueError("Invalid Dot metadata file.")
                provider = json.loads(raw)
                command = provider.get("command") if isinstance(provider, dict) else None
                if not (isinstance(command, list) and len(command) > 1 and command[1] == "dot"):
                    raise ValueError("A custom Dot provider is configured; it was preserved.")
                program = shutil.which("cepheus")
                if program is None:
                    raise ValueError("The matching Cepheus binary is required to select native Dot repository sources.")
                native = [program, "dot", "repo-sources", "--config-dir", str(config), "--source-root", str(ROOT)]
                if args.dry_run:
                    native.append("--dry-run")
                subprocess.run(native, check=True)
                return 0
            current_profile = path.read_bytes() if path.exists() else None
            if current_profile != profile_bytes:
                raise ValueError("The local profile changed during validation; retry.")
            if args.dry_run:
                print(f"{path}: plugin_source_dir = {ROOT}")
                return 0
            config.mkdir(parents=True, exist_ok=True)
            if path.exists():
                shutil.copy2(path, path.with_name(f"profile.json.before-plugin-update-{uuid.uuid4().hex}"))
            with tempfile.NamedTemporaryFile(prefix=".plugin-profile-", dir=config, delete=False) as stream:
                temporary = Path(stream.name)
                stream.write(data)
            try:
                os.replace(temporary, path)
            finally:
                temporary.unlink(missing_ok=True)
            print(f"Repository plugin sources: {ROOT}. Reopen the deck; service plugins need a service update or handover.")
            return 0
        names = (
            [name for name, plugin in plugins.items() if plugin["kind"] in ("optional", "helper")]
            if args.all
            else args.names
        )
        if not names:
            raise ValueError("Choose a plugin name, --all, --list, or --check")
        unknown = set(names) - plugins.keys()
        if unknown:
            raise ValueError(f"Unknown plugins: {', '.join(sorted(unknown))}")
        config = args.config_dir.expanduser().resolve()
        install(
            plugins, with_requirements(plugins, names), config, update=args.update, dry_run=args.dry_run
        )
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"Plugin install: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
