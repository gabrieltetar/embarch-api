---
name: embarch-hil
description: How to work with hardware through EmbArch - load it when a task needs a board, a flash, a serial log, a measurement, a conformance check against a specification, or a poke at a register. It holds the table for picking a mode (Live, Study or Test) and points at the guide for each.
---

# Hardware in the loop

Interactive sessions only. A fleet or background agent never touches hardware.

## Pick a mode

| The request | Mode |
|---|---|
| Names a specification or conformance: "does the PD contract work", "check R3.2" | **Test** |
| A custom question whose answer is a measurement: "what rate does it sustain", "how long does wake-up take" | **Study** |
| Look or poke while developing: "what is in this register", "reset it and show me the log" | **Live** |
| A Live result about to be cited in a PR, a ticket or a report | Redo it as a **Study** or a **Test**, so it has a published receipt |

A spec named is a Test, not a Study. A request that fits no row gets one question: Study, Test or Live, each in one line, your pick first.

## Read the guide for the mode

The guide is served by the binary, so it matches the tools you have and needs no `embarch-doc` checkout:

- `embarch-api guide hil` prints the "which mode" part.
- `embarch-api guide hil live`
- `embarch-api guide hil study`
- `embarch-api guide hil test`

Read the section before the first hardware call. If a section says it is not written yet, say so to the engineer and stop short of inventing a procedure.
