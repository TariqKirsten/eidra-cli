#!/usr/bin/env python3
"""A small to-do list, kept in a JSON file."""

import argparse
import json
import os
import sys

DEFAULT_FILE = "todo.json"


def load(path):
    if not os.path.exists(path):
        return []
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def save(path, items):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(items, f, indent=2)
        f.write("\n")


def add(items, text):
    next_id = max((item["id"] for item in items), default=0) + 1
    items.append({"id": next_id, "text": text, "done": False})
    return next_id


def mark_done(items, item_id):
    for item in items:
        if item["id"] == item_id:
            item["done"] = True
            return True
    return False


def format_item(item):
    box = "x" if item["done"] else " "
    return f"{item['id']:>3}. [{box}] {item['text']}"


def build_parser():
    parser = argparse.ArgumentParser(description="A small to-do list.")
    parser.add_argument("--file", default=DEFAULT_FILE, help="the list's JSON file (default: todo.json)")
    commands = parser.add_subparsers(dest="command", required=True)
    add_command = commands.add_parser("add", help="add an item")
    add_command.add_argument("text")
    commands.add_parser("list", help="show the items")
    done_command = commands.add_parser("done", help="mark an item done")
    done_command.add_argument("id", type=int)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    items = load(args.file)
    if args.command == "add":
        item_id = add(items, args.text)
        save(args.file, items)
        print(f"Added {item_id}.")
    elif args.command == "list":
        if not items:
            print("Nothing to do.")
        for item in items:
            print(format_item(item))
    elif args.command == "done":
        if not mark_done(items, args.id):
            print(f"error: there is no item {args.id}", file=sys.stderr)
            return 1
        save(args.file, items)
        print(f"Done {args.id}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
