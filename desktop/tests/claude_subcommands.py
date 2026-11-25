"""Fail when stock Claude Code has a root command OX does not know.

`ox claude` adds `--session-id` to a new session, but never to a subcommand
such as `mcp` or `update`. It recognises subcommands from CLAUDE_SUBCOMMANDS in
src/clients.rs, the only copy of that list. This check reads `claude --help` on
stdin, collects every root command and alias from its Commands section, and
fails if any is missing from that list. Hidden commands do not appear in the
help and may stay in the list.

Usage: claude --help | python3 desktop/tests/claude_subcommands.py src/clients.rs
"""
import re
import sys


def known(source):
    match = re.search(r"const CLAUDE_SUBCOMMANDS: &\[&str\] = &\[(.*?)\];", source, re.S)
    if not match:
        sys.exit("FAIL: CLAUDE_SUBCOMMANDS not found in src/clients.rs")
    names = re.findall(r'"([^"]+)"', match.group(1))
    if not names:
        sys.exit("FAIL: CLAUDE_SUBCOMMANDS is empty")
    return set(names)


def commands(help_text):
    """Root command names and aliases from a commander `Commands:` section."""
    found = []
    section = False
    for line in help_text.splitlines():
        if re.match(r"^Commands:\s*$", line):
            section = True
            continue
        if not section:
            continue
        if not line.strip():
            if found:
                break
            continue
        if not line.startswith(" "):
            break  # the next heading
        # Command rows have a two-space indent. Wrapped descriptions are deeper.
        row = re.match(r"^ {2}(\S+)", line)
        if row and not line.startswith("   "):
            found.extend(n for n in row.group(1).split("|") if n)
    return found


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip().splitlines()[-1])
    with open(sys.argv[1], encoding="utf-8") as f:
        listed = known(f.read())
    help_text = sys.stdin.read()
    found = commands(help_text)
    if not found:
        sys.exit("FAIL: no Commands section in `claude --help`; the help format changed")
    missing = sorted(set(found) - listed)
    if missing:
        sys.exit(
            "FAIL: add these root commands of `claude --help` to CLAUDE_SUBCOMMANDS in "
            "src/clients.rs, or ox claude will give them a --session-id: " + ", ".join(missing)
        )
    print(f"PASS: all {len(set(found))} root commands and aliases are in CLAUDE_SUBCOMMANDS")


if __name__ == "__main__":
    main()
