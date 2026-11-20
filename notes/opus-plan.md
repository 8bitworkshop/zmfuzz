Yes. I read through the code and notes, and decoded part of minizork.z3 to check. Right now the fuzzer only looks at the game from outside: the text it prints, plus object moves and attribute/property changes. It reads the dictionary as a plain list of words, and every Z-code idea in notes/ is about save-states. The story file has a lot more structure the search could use. Here are the options, most valuable per line of code first.

1. Dictionary word types (cheap, measured)

Each entry in a v3 dictionary has a flags byte saying what kind of word it is. I decoded Mini-Zork's:

┌──────────────────────────────┬───────┬─────────────────────────┐
│            flags             │ count │        examples         │
├──────────────────────────────┼───────┼─────────────────────────┤
│ 0x41 verb                    │   201 │ answer, apply, activa…  │
├──────────────────────────────┼───────┼─────────────────────────┤
│ 0x80 noun                    │   178 │ altar, axe, bag, basket │
├──────────────────────────────┼───────┼─────────────────────────┤
│ 0x22 adjective               │    76 │ brass, black, ancien…   │
├──────────────────────────────┼───────┼─────────────────────────┤
│ 0x13 / 0x18 / 0x33 direction │    25 │ n, east, up, in         │
├──────────────────────────────┼───────┼─────────────────────────┤
│ 0x08 preposition             │    21 │ at, behind, across      │
├──────────────────────────────┼───────┼─────────────────────────┤
│ 0x04 filler word             │    21 │ a, the, all, again      │
└──────────────────────────────┴───────┴─────────────────────────┘

Picking 2 random words from 536 gives a verb-then-noun command only about 13% of the time. If getrandomcmd and the template-slot filling in template_cmd respect word types, almost every command has the right shape, and filler words stop wasting turns. This is mostly a change to makevocab, plus reading the dictionary layout from the story file. That layout is fixed for Infocom v3; Inform games use the same flags byte with different bits, so this needs a per-compiler table.

2. Grammar tables

The verb entries also point to a verb number, which indexes the game's syntax tables. Those list the valid sentence patterns, such as PUT obj IN obj or TIE obj TO obj. With them you can generate only commands the parser accepts, including 3–4 word commands with prepositions that random search almost never finds. This overlaps with roadmap stage 0b (filling in missing slots), but it gets the answer from the file instead of learning it from refusals. Infocom and Inform lay these tables out differently, so it's two parsers, maybe 200 lines each.

3. Code coverage as a progress signal (the AFL idea, biggest change)

Today a turn only counts as progress if it prints new text or changes an object. Many important turns change only globals or local state. Your notes say @store_* tokens are deliberately off because they would be too noisy. Counting every branch's taken/not-taken outcome as a token gives a much finer signal that isn't noisy in the same way: it shows the game reaching a new part of its logic even when nothing visible happens.
- VM change: a VmEvent::Branch { pc, taken } in process_branch in encrusted-heart, or a cheaper bitmap.
- Search change: branch tokens become goals with the same first/best machinery. They would probably need to be kept out of the goal queue at first, as you did for ambient moves, because there will be thousands. A first version could use them only to credit a turn as useful.

4. Learning from comparisons (like AFL's cmplog/Redqueen)

Record the operands of failed tests: je, test_attr, jin, get_prop (already emitted) and jz. When the game compares the parsed noun against a specific dictionary address or object number, you learn which nouns this code path cares about. A failed test_attr obj 13 or jin obj player is exactly the requires(...) fact near-miss.md plans to recover by reading refusal text. It's more precise and works in any language.

5. Static string inventory (cheap, for measuring)

Disassemble the story and collect every print/print_ret string and every print_paddr target. That gives a denominator: "312 of 1,140 reachable strings seen". It also lets you match an output token to the routine that prints it, and tells you when a game is mostly explored.

Suggested order

Do 1 first. It's an afternoon, and tools/bench.sh should show it on useful_command_rate right away. Then 3, the VM branch event with turn crediting only, because it changes what "progress" means and the snapshot stages would benefit from it. 2 and 4 are where the bigger gains are, but each needs more VM/parser work.
