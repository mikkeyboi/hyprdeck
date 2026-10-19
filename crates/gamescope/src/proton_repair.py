"""Plan a narrow prefix-version parser repair; never execute a runner's source."""
import ast
import json
import re
import sys


VERSION = re.compile(r"(?:[A-Za-z][A-Za-z0-9_.-]*-)?[0-9]+\.[0-9]+-[0-9]+\Z")


def same(node, expression):
    return ast.dump(node, include_attributes=False) == ast.dump(
        ast.parse(expression).body[0], include_attributes=False
    )


def plan(source):
    try:
        tree = ast.parse(source)
    except (SyntaxError, ValueError, RecursionError):
        return {"kind": "unsupported", "reason": "Runner is not recognized Python source."}
    versions = [
        node.value.value
        for node in tree.body
        if isinstance(node, ast.Assign)
        and len(node.targets) == 1
        and isinstance(node.targets[0], ast.Name)
        and node.targets[0].id == "CURRENT_PREFIX_VERSION"
        and isinstance(node.value, ast.Constant)
        and isinstance(node.value.value, str)
    ]
    if len(versions) != 1 or not VERSION.fullmatch(versions[0]):
        return {"kind": "unsupported", "reason": "Runner's prefix-version format is unknown; no repair is offered."}
    methods = [
        method
        for cls in tree.body
        if isinstance(cls, ast.ClassDef)
        for method in cls.body
        if isinstance(method, ast.FunctionDef) and method.name == "upgrade_pfx"
    ]
    if len(methods) != 1:
        return {"kind": "unsupported", "reason": "Runner's migration method is unknown; no repair is offered."}
    original = [
        "old_proton_ver, old_prefix_ver = old_ver.split('-')",
        "old_proton_maj, old_proton_min = old_proton_ver.split('.')",
        "new_proton_ver, _ = CURRENT_PREFIX_VERSION.split('-')",
        "new_proton_maj, new_proton_min = new_proton_ver.split('.')",
    ]
    repaired = [
        "old_proton_ver, old_prefix_ver = old_ver.rsplit('-', 1)",
        "old_proton_ver = old_proton_ver.rsplit('-', 1)[-1]",
        original[1],
        "new_proton_ver, _ = CURRENT_PREFIX_VERSION.rsplit('-', 1)",
        "new_proton_ver = new_proton_ver.rsplit('-', 1)[-1]",
        original[3],
    ]
    candidates = [node for node in ast.walk(methods[0]) if isinstance(node, ast.Try)]
    matches = [node for node in candidates if len(node.body) >= 4 and all(same(node.body[i], expression) for i, expression in enumerate(original))]
    if len(matches) != 1:
        fixed = [node for node in candidates if len(node.body) >= 6 and all(same(node.body[i], expression) for i, expression in enumerate(repaired))]
        if len(fixed) == 1:
            return {"kind": "fixed", "version": versions[0]}
        return {"kind": "unsupported", "reason": "No exact affected migration parser was found. Upstream may already have changed it; the runner is left untouched."}
    nodes = matches[0].body[:4]
    # Preserve all original bytes outside these two complete assignment statements.
    lines = source.splitlines(keepends=True)
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line.encode("utf-8")))
    data = source.encode("utf-8")
    edits = []
    for index, first in [(0, 0), (2, 3)]:
        node = nodes[index]
        line = lines[node.lineno - 1]
        indent = line.encode("utf-8")[:node.col_offset].decode("utf-8")
        newline = "\r\n" if line.endswith("\r\n") else "\n"
        body = (repaired[first] + newline + indent + repaired[first + 1]).encode("utf-8")
        start = offsets[node.lineno - 1] + node.col_offset
        end = offsets[node.end_lineno - 1] + node.end_col_offset
        edits.append((start, end, body))
    for start, end, body in reversed(edits):
        data = data[:start] + body + data[end:]
    changed = data.decode("utf-8")
    # Compilation checks syntax only: no imported or downloaded code is executed.
    compile(changed, "<proton-prefix-parser>", "exec", dont_inherit=True)
    return {"kind": "affected", "version": versions[0], "patched": changed}


if __name__ == "__main__":
    try:
        source = sys.stdin.buffer.read(1024 * 1024 + 1)
        if len(source) > 1024 * 1024:
            raise ValueError("Runner source exceeds the repair inspection limit.")
        print(json.dumps(plan(source.decode("utf-8"))))
    except (UnicodeError, ValueError, SyntaxError, RecursionError) as error:
        print(json.dumps({"kind": "unsupported", "reason": str(error)}))
