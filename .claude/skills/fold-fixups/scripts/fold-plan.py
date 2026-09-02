#!/usr/bin/env python3
"""Where each unpushed commit's changes come from — the input to a fold plan.

For every commit in BASE..HEAD, blames the lines each hunk touches (the removed
lines, or the two lines around a pure insertion) at the commit's parent, and
counts which earlier commits those lines came from. Read the result with the
commit's INTENT in mind:

* a fix-up commit whose lines all come from commits inside BASE..HEAD belongs
  in those commits — fold it there;
* a fix-up whose lines come from BASE or earlier corrects code that predates
  the branch — it stays a standalone commit (split it off if the same commit
  also carries fold-able hunks);
* a feature commit naturally touches old lines and new files; the blame says
  nothing about whether it should move.

usage: fold-plan.py [BASE] [--hunks]     (BASE defaults to main)
Read-only.
"""
import re
import subprocess
import sys


def git(*args):
	return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def short(sha):
	return sha[:8]


def main():
	args = [a for a in sys.argv[1:] if not a.startswith("--")]
	show_hunks = "--hunks" in sys.argv
	base = args[0] if args else "main"
	commits = git("rev-list", "--reverse", f"{base}..HEAD").split()
	if not commits:
		print(f"nothing on top of {base}")
		return
	in_range = set(commits)
	subject = {c: git("log", "-1", "--format=%s", c).strip() for c in commits}
	print(f"{len(commits)} commits on top of {short(git('rev-parse', base).strip())} ({base})\n")

	for c in commits:
		parent = git("rev-parse", f"{c}^").strip()
		diff = git("diff", "-U0", parent, c)
		print(f"{short(c)} {subject[c]}")
		file = None
		lines_from = {}
		new_files = 0
		for line in diff.splitlines():
			if line.startswith("+++ "):
				file = None if line[4:] == "/dev/null" else line[6:]
				continue
			m = re.match(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", line)
			if not m or file is None:
				continue
			old_start, old_len = int(m.group(1)), int(m.group(2) or "1")
			new_start, new_len = int(m.group(3)), int(m.group(4) or "1")
			if old_len == 0:
				lo, hi = max(old_start, 1), old_start + 1
			else:
				lo, hi = old_start, old_start + old_len - 1
			try:
				blame = git("blame", "-l", "-L", f"{lo},{hi}", parent, "--", file)
			except subprocess.CalledProcessError:
				new_files += 1
				if show_hunks:
					print(f"    {file}:{new_start}+{new_len}  new file")
				continue
			owners = {}
			for b in blame.splitlines():
				sha = git("rev-parse", b.split(" ", 1)[0].lstrip("^")).strip()
				key = sha if sha in in_range else "_outside"
				owners[key] = owners.get(key, 0) + 1
				lines_from[key] = lines_from.get(key, 0) + 1
			if show_hunks:
				named = ", ".join(
					f"{short(k)} ({n})" if k != "_outside" else f"{base}-or-earlier ({n})"
					for k, n in sorted(owners.items(), key=lambda kv: -kv[1])
				)
				print(f"    {file}:{new_start}+{new_len}  <- {named}")
		inside = {k: n for k, n in lines_from.items() if k != "_outside"}
		outside = lines_from.get("_outside", 0)
		parts = [f"{n} lines from {short(k)} {subject[k]}" for k, n in sorted(inside.items(), key=lambda kv: -kv[1])]
		if outside:
			parts.append(f"{outside} lines from {base} or earlier")
		if new_files:
			parts.append(f"{new_files} hunks in new files")
		for p in parts:
			print(f"  {p}")
		print()


if __name__ == "__main__":
	main()
