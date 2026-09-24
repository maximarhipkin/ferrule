"""List the tasks in tasks.txt: `[x] title` lines are done, `[ ] title` aren't."""
import sys


def load(path="tasks.txt"):
    tasks = []
    for line in open(path):
        line = line.rstrip("\n")
        if line.startswith("[x] ") or line.startswith("[ ] "):
            tasks.append((line[4:], line[1] == "x"))
    return tasks


def main(argv):
    for i, (title, done) in enumerate(load(), 1):
        print(f"{i}. {'✓' if done else ' '} {title}")


if __name__ == "__main__":
    main(sys.argv[1:])
