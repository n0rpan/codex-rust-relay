"""Keep the stock Claude session-history reader narrow and read-only."""
import re
import tomllib
from pathlib import Path


def check(desktop: Path):
    allowed = tomllib.loads((desktop / "ci/migration.toml").read_text())["stock_read_only"]
    reader = "claude/catalog.rs"
    if set(allowed) != {reader}:
        raise SystemExit("The Claude catalog is the sole stock history filesystem exception")
    for source in (desktop / "ox-desk-core/src").rglob("*.rs"):
        relative = source.relative_to(desktop / "ox-desk-core/src").as_posix()
        code = source.read_text().split("#[cfg(test)]", 1)[0]
        if relative == reader:
            writes = r"(?:\.(?:write|append|create|truncate)\(true\)|(?:fs|File)::(?:write|create\w*|remove\w*|rename|copy)|write_all\()"
            if re.search(writes, code):
                raise SystemExit("Stock Claude catalog must stay read-only")
        elif re.search(r'"(?:\.claude|subagents|[^"]*\.jsonl)"', code) and (
            "read_dir(" in code or "File::open(" in code or "OpenOptions::" in code
        ):
            raise SystemExit(f"{relative} may not read stock Claude history")
