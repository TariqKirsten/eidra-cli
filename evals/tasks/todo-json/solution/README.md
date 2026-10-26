# todo

A small to-do list for the command line, kept in a JSON file.

## Usage

    python3 todo.py add "buy milk"     # add an item
    python3 todo.py list               # show every item
    python3 todo.py list --pending     # show only the items not done
    python3 todo.py list --json        # print the items as a JSON array
    python3 todo.py done 1             # mark item 1 done

`--pending` and `--json` combine: `list --pending --json` prints the pending items as JSON. Each
item is an object with the keys `id`, `text` and `done`.

Every command takes `--file PATH` before it, to use another list than `todo.json`:

    python3 todo.py --file work.json list

## Tests

    python3 -m unittest -q
