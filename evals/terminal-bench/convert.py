#!/usr/bin/env python3
"""Turn Terminal-Bench-style tasks into a `ferrule eval` suite (M58.5,
docs/eval.md, "An external benchmark"). Standard library only; nothing is
downloaded: point it at a checkout you already have.

    python3 evals/terminal-bench/convert.py TASKS_DIR OUT_DIR [--only a,b] [--limit N] [--with-builds]

Two layouts are read, one task per directory under TASKS_DIR:

  Terminal-Bench 2 (Harbor): instruction.md, task.toml, environment/
      (Dockerfile and the files it copies), tests/ (test.sh and
      test_outputs.py), solution/solve.sh
  Terminal-Bench 1: task.yaml (`instruction:`), Dockerfile and the files
      beside it, tests/test_outputs.py, run-tests.sh, solution.sh

A task becomes a `[[task]]`:

  - the prompt is the instruction;
  - the fixture is the environment's files, without the Dockerfile and
    compose files;
  - the grader is `python3 -m pytest` on the task's test_outputs.py,
    copied to OUT_DIR/graders/<id>/ (outside the workspace, where the
    eval's tripwire hides it from the agent);
  - the reference solution goes to OUT_DIR/solutions/<id>/solve.sh.

The tasks run in a container whose working directory is /app; ferrule's
workspace stands in for it. `/app/…` in the instruction, the tests and
the solution becomes a path relative to the workspace (the grader runs
there), and `/tests/…` in the tests becomes their copy's directory.

A task whose image does more than copy files in needs that image: one
with RUN steps is skipped and said so, unless --with-builds (then this
machine must already have what they install), and one whose compose file
starts more than one service always is. So is one without
test_outputs.py. What's left grades on the host and needs python3 with
pytest.
"""

import argparse
import json
import re
import shutil
import sys
from pathlib import Path

try:
    import tomllib
except ImportError:  # Python < 3.11: task.toml's timeouts are skipped
    tomllib = None

APP = re.compile(r"/app(?=/|\b)")
DOCKER_FILES = {"Dockerfile", "docker-compose.yaml", "docker-compose.yml", ".dockerignore"}


def yaml_instruction(text):
    """`instruction:` (or TB1's first `description:`) from task.yaml: a
    block scalar or a one-line value."""
    lines = text.splitlines()
    for key in ("instruction", "description"):
        for i, line in enumerate(lines):
            m = re.match(rf"^(\s*)(?:- )?{key}:\s*(.*)$", line)
            if not m:
                continue
            indent, value = len(m.group(1)), m.group(2).strip()
            if value and value[0] not in "|>":
                return value.strip("\"'")
            block = []
            for nxt in lines[i + 1:]:
                if nxt.strip() and len(nxt) - len(nxt.lstrip()) <= indent:
                    break
                block.append(nxt)
            pad = min((len(b) - len(b.lstrip()) for b in block if b.strip()), default=0)
            body = [b[pad:] for b in block]
            joiner = " " if value.startswith(">") else "\n"
            return joiner.join(body).strip()
    return None


def read_task(task):
    """(instruction, environment dir, tests dir, solution file, timeout) or
    a reason to skip."""
    if (task / "instruction.md").is_file():
        timeout = None
        if tomllib and (task / "task.toml").is_file():
            meta = tomllib.loads((task / "task.toml").read_text())
            timeout = meta.get("agent", {}).get("timeout_sec")
        env = task / "environment"
        return ((task / "instruction.md").read_text(), env if env.is_dir() else None,
                task / "tests", task / "solution" / "solve.sh", timeout)
    if (task / "task.yaml").is_file():
        text = (task / "task.yaml").read_text()
        instruction = yaml_instruction(text)
        if not instruction:
            return "task.yaml has no instruction"
        m = re.search(r"^max_agent_timeout_sec:\s*(\d+)", text, re.M)
        return (instruction, task, task / "tests", task / "solution.sh",
                int(m.group(1)) if m else None)
    return "neither instruction.md nor task.yaml"


def needs_image(env, builds):
    """Why the environment can't be stood in for by a copy of its files."""
    if env is None:
        return None
    docker = env / "Dockerfile"
    if docker.is_file() and not builds:
        for line in docker.read_text().splitlines():
            word = line.strip().split(" ", 1)[0].upper()
            if word == "RUN":
                return "its image runs RUN steps (--with-builds takes it anyway)"
    for name in ("docker-compose.yaml", "docker-compose.yml"):
        compose = env / name
        if compose.is_file() and len(re.findall(r"^\s{2}\w[\w-]*:\s*$", compose.read_text(), re.M)) > 1:
            return "its compose file starts more than one service"
    return None


def rewrite(text, tests_dir=None):
    text = APP.sub(".", text)
    if tests_dir is not None:
        text = text.replace("/tests/", f"{tests_dir}/")
    return text


def copy_rewritten(src, dst, tests_dir=None):
    dst.mkdir(parents=True, exist_ok=True)
    for f in src.rglob("*"):
        if f.is_dir() or f.name in DOCKER_FILES:
            continue
        to = dst / f.relative_to(src)
        to.parent.mkdir(parents=True, exist_ok=True)
        try:
            to.write_text(rewrite(f.read_text(), tests_dir))
        except UnicodeDecodeError:
            shutil.copy2(f, to)


def toml_str(s):
    # A JSON string is a valid TOML basic string.
    return json.dumps(s, ensure_ascii=False)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("tasks", type=Path, help="the directory of task directories")
    ap.add_argument("out", type=Path, help="where the suite goes (made if missing)")
    ap.add_argument("--only", default="", help="comma-separated task ids")
    ap.add_argument("--limit", type=int, default=0, help="at most this many tasks")
    ap.add_argument("--name", default="terminal-bench", help="the suite's name")
    ap.add_argument("--with-builds", action="store_true",
                    help="keep tasks whose image has RUN steps")
    args = ap.parse_args()
    only = {t for t in args.only.split(",") if t}
    out = args.out.resolve()
    entries, skipped = [], []
    for task in sorted(p for p in args.tasks.iterdir() if p.is_dir()):
        tid = re.sub(r"[^A-Za-z0-9_-]", "-", task.name)
        if only and task.name not in only and tid not in only:
            continue
        if args.limit and len(entries) >= args.limit:
            break
        read = read_task(task)
        if isinstance(read, str):
            skipped.append((task.name, read))
            continue
        instruction, env, tests, solution, timeout = read
        why = needs_image(env, args.with_builds)
        if why is None and not (tests / "test_outputs.py").is_file():
            why = "no tests/test_outputs.py"
        if why:
            skipped.append((task.name, why))
            continue
        graders = out / "graders" / tid
        copy_rewritten(tests, graders, tests_dir=graders)
        if env is not None:
            fixture = out / "fixtures" / tid
            copy_rewritten(env, fixture)
            for sub in ("tests", "solution"):
                shutil.rmtree(fixture / sub, ignore_errors=True)
            for name in ("task.yaml", "run-tests.sh", "solution.sh", "solution.yaml"):
                (fixture / name).unlink(missing_ok=True)
        if solution.is_file():
            (out / "solutions" / tid).mkdir(parents=True, exist_ok=True)
            (out / "solutions" / tid / "solve.sh").write_text(rewrite(solution.read_text()))
        lines = ["", "[[task]]", f"id = {toml_str(tid)}", 'tags = ["tb"]']
        if env is not None and any((out / "fixtures" / tid).iterdir()):
            lines.append(f'fixture = "fixtures/{tid}"')
        if timeout:
            lines.append(f"timeout_secs = {int(timeout)}")
        lines.append(f"prompt = {toml_str(rewrite(instruction).strip())}")
        lines += ["[task.grade]",
                  f"command = 'python3 -m pytest -q -p no:cacheprovider \"{{suite_dir}}/graders/{tid}/test_outputs.py\"'"]
        entries.append("\n".join(lines))
    out.mkdir(parents=True, exist_ok=True)
    header = [
        f"# Converted by evals/terminal-bench/convert.py from {args.tasks.resolve()}.",
        "# Graders need python3 with pytest. See docs/eval.md, \"An external benchmark\".",
        "",
        "[suite]",
        f"name = {toml_str(args.name)}",
        'kind = "capability"',
        'description = "Terminal-Bench-style tasks that run without their image"',
        "max_iterations = 60",
        "timeout_secs = 1800",
    ]
    (out / "suite.toml").write_text("\n".join(header + entries) + "\n")
    print(f"{len(entries)} task(s) written to {out / 'suite.toml'}")
    for name, why in skipped:
        print(f"  skipped {name}: {why}")
    return 0 if entries else 1


if __name__ == "__main__":
    sys.exit(main())
